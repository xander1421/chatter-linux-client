//! The payload formats carried inside bitchat packets.
//!
//! Every module here ports one Swift type byte-for-byte. They share nothing but
//! the cursor helpers below, because the formats genuinely disagree with each
//! other: `AnnouncementPacket` writes 1-byte TLV lengths, `CourierEnvelope`
//! writes 2-byte big-endian ones, and `BitchatFilePacket` writes 2 bytes
//! for metadata and 4 for content. Do not "unify" them.
//!
//! # Uniform surface
//!
//! ```ignore
//! impl X {
//!     pub fn encode(&self) -> Result<Vec<u8>, MeshError>;
//!     pub fn decode(bytes: &[u8]) -> Result<Self, MeshError>;
//! }
//! ```
//!
//! # Deliberate divergences from Swift
//!
//! The Swift decoders are `Optional`-returning, and several of them use
//! `String(data:encoding:)` in a position where a `nil` (invalid UTF-8) silently
//! drops an optional field, or substitutes a fallback like `"unknown"` or a fresh
//! `UUID().uuidString`. This port returns [`MeshError::InvalidUtf8`] at every one
//! of those sites instead. Two reasons: a sans-IO crate cannot mint a random UUID,
//! and silently accepting a field we could not read is precisely the behaviour a
//! parser exposed to hostile radio input should not have. A real bitchat client
//! never emits invalid UTF-8 in these fields — they originate as Swift `String`s —
//! so the divergence is unreachable for honest peers. Each site is commented.

pub mod announce;
pub mod capabilities;
pub mod courier_envelope;
pub mod file;
pub mod file_stream;
pub mod private_message;
pub mod request_sync;

use crate::error::MeshError;

/// Bounds-checked forward cursor over a caller-supplied buffer.
///
/// Every read either advances the cursor by exactly the amount consumed or
/// returns [`MeshError::Truncated`] and leaves the cursor untouched. Nothing here
/// indexes, so nothing here panics.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Byte offset of the next unread byte. Used for error reporting.
    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Consumes `n` bytes, or fails without moving the cursor.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], MeshError> {
        let end = self.pos.checked_add(n).ok_or(MeshError::Truncated {
            offset: self.pos,
            needed: n,
        })?;
        let slice = self.buf.get(self.pos..end).ok_or(MeshError::Truncated {
            offset: self.pos,
            needed: n.saturating_sub(self.remaining()),
        })?;
        self.pos = end;
        Ok(slice)
    }

    /// Consumes everything left, possibly nothing.
    pub fn take_rest(&mut self) -> &'a [u8] {
        let slice = self.buf.get(self.pos..).unwrap_or(&[]);
        self.pos = self.buf.len();
        slice
    }

    /// Everything left, without consuming it.
    pub fn peek_rest(&self) -> &'a [u8] {
        self.buf.get(self.pos..).unwrap_or(&[])
    }

    /// Rewinds to a previously recorded [`Reader::position`].
    ///
    /// Only `BitchatFilePacket` needs this: its content TLV is read as a 4-byte
    /// length and retried as a 2-byte one when the 4-byte read is implausible.
    pub fn seek(&mut self, pos: usize) {
        self.pos = pos.min(self.buf.len());
    }

    pub fn u8(&mut self) -> Result<u8, MeshError> {
        let b = self.take(1)?;
        Ok(b.first().copied().unwrap_or(0))
    }

    pub fn u16_be(&mut self) -> Result<u16, MeshError> {
        let b: [u8; 2] = self.take(2)?.try_into().map_err(|_| MeshError::Truncated {
            offset: self.pos,
            needed: 2,
        })?;
        Ok(u16::from_be_bytes(b))
    }

    pub fn u32_be(&mut self) -> Result<u32, MeshError> {
        let b: [u8; 4] = self.take(4)?.try_into().map_err(|_| MeshError::Truncated {
            offset: self.pos,
            needed: 4,
        })?;
        Ok(u32::from_be_bytes(b))
    }

    pub fn u64_be(&mut self) -> Result<u64, MeshError> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| MeshError::Truncated {
            offset: self.pos,
            needed: 8,
        })?;
        Ok(u64::from_be_bytes(b))
    }

    /// Consumes exactly `N` bytes into a fixed-width array.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], MeshError> {
        let pos = self.pos;
        self.take(N)?
            .try_into()
            .map_err(|_| MeshError::Truncated {
                offset: pos,
                needed: N,
            })
    }
}

/// Decodes a TLV value that must be UTF-8.
pub fn utf8(bytes: &[u8], field: &'static str) -> Result<String, MeshError> {
    core::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| MeshError::InvalidUtf8 { field })
}

/// Reinterprets a byte slice as a fixed-width field, or reports its real width.
pub fn fixed<const N: usize>(bytes: &[u8], field: &'static str) -> Result<[u8; N], MeshError> {
    bytes.try_into().map_err(|_| MeshError::BadFieldLength {
        field,
        actual: bytes.len(),
        expected: N,
    })
}

/// `tag ‖ len:u8 ‖ value` — the `AnnouncementPacket` / `PrivateMessagePacket` shape.
pub fn put_tlv_u8_len(out: &mut Vec<u8>, tag: u8, value: &[u8]) -> Result<(), MeshError> {
    if value.len() > u8::MAX as usize {
        return Err(MeshError::LengthExceeded {
            declared: value.len(),
            max: u8::MAX as usize,
        });
    }
    out.push(tag);
    out.push(value.len() as u8);
    out.extend_from_slice(value);
    Ok(())
}

/// `tag ‖ len:u16be ‖ value` — the `CourierEnvelope` / `RequestSync` shape.
pub fn put_tlv_u16_len(out: &mut Vec<u8>, tag: u8, value: &[u8]) -> Result<(), MeshError> {
    if value.len() > u16::MAX as usize {
        return Err(MeshError::LengthExceeded {
            declared: value.len(),
            max: u16::MAX as usize,
        });
    }
    out.push(tag);
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
    Ok(())
}

/// `tag ‖ len:u32be ‖ value` — used only by the file packet's content TLV.
pub fn put_tlv_u32_len(out: &mut Vec<u8>, tag: u8, value: &[u8]) -> Result<(), MeshError> {
    if value.len() > u32::MAX as usize {
        return Err(MeshError::LengthExceeded {
            declared: value.len(),
            max: u32::MAX as usize,
        });
    }
    out.push(tag);
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
    Ok(())
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Lowercase hex, matching `Data.hexEncodedString()`.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        out.push(HEX_DIGITS[(byte & 0x0F) as usize] as char);
    }
    out
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parses exactly `2 * N` hex digits. Rejects anything else — including the
/// `0x` prefix and surrounding whitespace that Swift's `Data(hexString:)` strips.
///
/// That strictness is deliberate. Swift's leniency combined with a caller that
/// only checks the *character* count lets `"0x" + 14 hex digits` parse to a
/// 7-byte value that then masquerades as an 8-byte fragment stream ID. A
/// fixed-width array cannot represent that, and should not.
pub fn hex_decode_exact<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes = text.as_bytes();
    if bytes.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_nibble(*bytes.get(i * 2)?)?;
        let lo = hex_nibble(*bytes.get(i * 2 + 1)?)?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_reads_every_width_big_endian() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let mut r = Reader::new(&buf);
        assert_eq!(r.u8().unwrap(), 0x01);
        assert_eq!(r.u16_be().unwrap(), 0x0203);
        assert_eq!(r.u32_be().unwrap(), 0x0405_0607);
        assert_eq!(r.remaining(), 2);
        assert_eq!(r.take_rest(), &[0x08, 0x09]);
        assert!(r.is_empty());
    }

    #[test]
    fn reader_u64_is_big_endian() {
        let buf = [0, 0, 0, 0, 0, 0, 0x01, 0x00];
        assert_eq!(Reader::new(&buf).u64_be().unwrap(), 256);
    }

    #[test]
    fn reader_truncation_does_not_move_the_cursor() {
        let buf = [0xAA];
        let mut r = Reader::new(&buf);
        assert!(matches!(
            r.u32_be(),
            Err(MeshError::Truncated { offset: 0, needed: 3 })
        ));
        assert_eq!(r.position(), 0);
        assert_eq!(r.u8().unwrap(), 0xAA);
        assert!(matches!(r.u8(), Err(MeshError::Truncated { offset: 1, .. })));
    }

    #[test]
    fn reader_take_rejects_overflowing_length() {
        let buf = [0u8; 4];
        let mut r = Reader::new(&buf);
        assert!(r.take(usize::MAX).is_err());
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn reader_seek_clamps_to_buffer() {
        let buf = [1, 2, 3];
        let mut r = Reader::new(&buf);
        r.seek(99);
        assert!(r.is_empty());
        r.seek(1);
        assert_eq!(r.take_rest(), &[2, 3]);
    }

    #[test]
    fn reader_array_is_fixed_width() {
        let buf = [1, 2, 3, 4];
        let mut r = Reader::new(&buf);
        assert_eq!(r.array::<2>().unwrap(), [1, 2]);
        assert!(r.array::<4>().is_err());
    }

    #[test]
    fn utf8_rejects_invalid_sequences() {
        assert_eq!(utf8(b"hello", "f").unwrap(), "hello");
        assert!(matches!(
            utf8(&[0xFF, 0xFE], "f"),
            Err(MeshError::InvalidUtf8 { field: "f" })
        ));
    }

    #[test]
    fn fixed_reports_the_real_width() {
        assert_eq!(fixed::<2>(&[1, 2], "f").unwrap(), [1, 2]);
        assert!(matches!(
            fixed::<2>(&[1, 2, 3], "f"),
            Err(MeshError::BadFieldLength {
                field: "f",
                actual: 3,
                expected: 2
            })
        ));
    }

    #[test]
    fn tlv_writers_emit_their_declared_length_width() {
        let mut out = Vec::new();
        put_tlv_u8_len(&mut out, 0x01, b"ab").unwrap();
        assert_eq!(out, vec![0x01, 0x02, b'a', b'b']);

        let mut out = Vec::new();
        put_tlv_u16_len(&mut out, 0x02, b"ab").unwrap();
        assert_eq!(out, vec![0x02, 0x00, 0x02, b'a', b'b']);

        let mut out = Vec::new();
        put_tlv_u32_len(&mut out, 0x03, b"ab").unwrap();
        assert_eq!(out, vec![0x03, 0x00, 0x00, 0x00, 0x02, b'a', b'b']);
    }

    #[test]
    fn tlv_u8_writer_rejects_oversized_values() {
        let mut out = Vec::new();
        assert!(matches!(
            put_tlv_u8_len(&mut out, 0x01, &[0u8; 256]),
            Err(MeshError::LengthExceeded {
                declared: 256,
                max: 255
            })
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn hex_round_trips_lowercase() {
        assert_eq!(hex_encode(&[0x00, 0xAB, 0xFF]), "00abff");
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(
            hex_decode_exact::<8>("0011223344556677"),
            Some([0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77])
        );
        // Uppercase parses, as Swift's does.
        assert_eq!(hex_decode_exact::<2>("ABcd"), Some([0xAB, 0xCD]));
    }

    #[test]
    fn hex_decode_rejects_wrong_width_and_non_hex() {
        assert_eq!(hex_decode_exact::<8>("001122334455667"), None);
        assert_eq!(hex_decode_exact::<8>("00112233445566778"), None);
        assert_eq!(hex_decode_exact::<8>("00112233445566zz"), None);
        assert_eq!(hex_decode_exact::<8>("0x00112233445566"), None);
        assert_eq!(hex_decode_exact::<8>(" 011223344556677"), None);
    }
}
