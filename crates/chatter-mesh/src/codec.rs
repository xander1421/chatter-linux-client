//! The bitchat binary packet framing.
//!
//! Ported from `BitFoundation/BinaryProtocol.swift`. This is the crate's entire
//! attack surface: [`decode`] runs on unauthenticated bytes handed to us by a
//! stranger's radio, before any signature or Noise session has vouched for
//! anything. It therefore length-checks every read, bounds every allocation on a
//! constant rather than on a field the sender chose, and never panics.
//!
//! # Wire format
//!
//! ```text
//! Header — 14 bytes (v1) / 16 bytes (v2), all integers big-endian:
//!   version:1  type:1  ttl:1  timestamp:8  flags:1  payloadLength:2 (v1) | 4 (v2)
//!
//! Body, in order, each present only when its flag is set:
//!   senderID:8
//!   recipientID:8      HAS_RECIPIENT
//!   routeCount:1 + hops:8*n   HAS_ROUTE   (v2 only)
//!   originalSize:2|4   IS_COMPRESSED
//!   payload:*
//!   signature:64       HAS_SIGNATURE
//! ```
//!
//! # Details that silently break interop
//!
//! * **`payloadLength` excludes the route bytes but includes the original-size
//!   field** when `IS_COMPRESSED` is set. Route bytes are framed but unaccounted.
//! * **`HAS_ROUTE` is v2-only.** A v1 frame that sets the bit must have the bit
//!   *ignored* — not rejected, and not parsed as a route. Rejecting partitions the
//!   mesh; parsing desynchronises the cursor and corrupts the payload.
//! * **Compression is raw DEFLATE at a 100-byte threshold** — see [`crate::compress`].
//! * **Trailing bytes are tolerated.** Swift's final bounds check is
//!   `offset <= count`, never `==`. This is what lets a *padded* frame decode on
//!   the first pass, with the pad bytes simply sitting unread past the signature.
//! * **[`decode`] parses the raw bytes first and only then retries after
//!   unpadding.** Reversing that order corrupts any payload whose final byte
//!   happens to look like a PKCS#7 marker.

use std::borrow::Cow;

use crate::compress;
use crate::error::MeshError;
use crate::packet::{BitchatPacket, SENDER_ID_LEN, SIGNATURE_LEN};
use crate::padding;

/// v1 header: version, type, ttl, timestamp(8), flags, payloadLength(2).
pub const V1_HEADER_SIZE: usize = 14;
/// v2 header: as v1 but with a 4-byte payloadLength.
pub const V2_HEADER_SIZE: usize = 16;
/// Recipient IDs are the same fixed width as sender IDs.
pub const RECIPIENT_ID_LEN: usize = SENDER_ID_LEN;
/// Byte offset of the flags field, identical in both versions.
///
/// Mirrors `BinaryProtocol.Offsets.flags`: after version(1) + type(1) + ttl(1) +
/// timestamp(8).
pub const FLAGS_OFFSET: usize = 11;
/// The hop count is a single byte, so a route cannot exceed 255 hops.
pub const MAX_ROUTE_HOPS: usize = 255;
/// Decompression above this ratio is treated as a bomb and rejected.
pub const MAX_COMPRESSION_RATIO: f64 = 50_000.0;

/// Header flag bits.
///
/// Bits 5-7 are unassigned. They are preserved by neither encoder nor decoder —
/// bitchat masks the bits it knows and drops the rest, and so do we.
pub mod flags {
    /// A recipient ID field follows the sender ID.
    pub const HAS_RECIPIENT: u8 = 0x01;
    /// A 64-byte Ed25519 signature trails the payload.
    pub const HAS_SIGNATURE: u8 = 0x02;
    /// The payload is raw-deflated and prefixed with its original size.
    pub const IS_COMPRESSED: u8 = 0x04;
    /// A source route follows the recipient. **v2 only** — ignored on v1.
    pub const HAS_ROUTE: u8 = 0x08;
    /// Request-Sync Response: exempt from the freshness window.
    pub const IS_RSR: u8 = 0x10;
}

// --- FileTransferLimits.maxFramedFileBytes, recomputed rather than guessed. ---
// Ported from `BitFoundation/FileTransferLimits.swift`. This is the decoder's
// pre-allocation ceiling, so it has to match theirs exactly or we reject frames
// they consider legal.
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
/// `fileName` + `mimeType` TLVs, each with a 16-bit length.
const MAX_METADATA_BYTES: usize = (u16::MAX as usize) * 2;
/// TLV tags and lengths, plus the metadata itself.
const TLV_ENVELOPE_OVERHEAD: usize = 18 + MAX_METADATA_BYTES;
/// Worst-case binary framing: v2 header + sender + recipient + signature.
const BINARY_ENVELOPE_OVERHEAD: usize =
    V2_HEADER_SIZE + SENDER_ID_LEN + RECIPIENT_ID_LEN + SIGNATURE_LEN;

/// Largest `payloadLength` the decoder will honour: 1_179_760 bytes.
///
/// Checked *before* any allocation sized from the frame — this is an anti-DoS
/// bound, not tidiness. A 4-byte v2 length field would otherwise let a 16-byte
/// frame ask us to reserve 4 GiB.
pub const MAX_FRAMED_FILE_BYTES: usize =
    MAX_PAYLOAD_BYTES + TLV_ENVELOPE_OVERHEAD + BINARY_ENVELOPE_OVERHEAD;

const fn header_size(version: u8) -> Option<usize> {
    match version {
        1 => Some(V1_HEADER_SIZE),
        2 => Some(V2_HEADER_SIZE),
        _ => None,
    }
}

/// v2 widened the length fields — both the header's `payloadLength` and the
/// compressed payload's original-size prefix — from 2 bytes to 4.
const fn length_field_size(version: u8) -> usize {
    if version == 2 { 4 } else { 2 }
}

/// Serialises a packet to its wire bytes.
///
/// With `padding` set the frame is rounded up to a [`crate::padding`] block, which
/// is the default everywhere in bitchat *including the signing preimage* — see
/// [`BitchatPacket::signing_preimage`]. Pass `false` only when you specifically
/// want the unpadded frame.
///
/// Compression is applied automatically when [`compress::should_compress`] says so
/// and deflate actually shrinks the payload; the caller has no say, because the
/// receiving peer reconstructs the decision from the flag alone.
pub fn encode(packet: &BitchatPacket, padding: bool) -> Result<Vec<u8>, MeshError> {
    let version = packet.version;
    if version != 1 && version != 2 {
        return Err(MeshError::UnsupportedVersion(version));
    }
    let length_field_bytes = length_field_size(version);
    let Some(header_size) = header_size(version) else {
        return Err(MeshError::UnsupportedVersion(version));
    };

    // Compress when beneficial, keeping the original size for the decoder.
    let mut payload: Cow<'_, [u8]> = Cow::Borrowed(&packet.payload);
    let mut original_payload_size: Option<usize> = None;
    if compress::should_compress(&packet.payload) {
        // Only compress when the original length is representable in this
        // version's length field.
        let max_representable = if version == 2 {
            u32::MAX as usize
        } else {
            u16::MAX as usize
        };
        if packet.payload.len() <= max_representable {
            if let Some(compressed) = compress::compress(&packet.payload) {
                original_payload_size = Some(packet.payload.len());
                payload = Cow::Owned(compressed);
            }
        }
    }
    let is_compressed = original_payload_size.is_some();

    // Routes are v2+ only, per SOURCE_ROUTING.md. A v1 packet carrying a route in
    // memory encodes without it and without the flag — silently, exactly as Swift
    // does, because the origination policy attaches routes before it knows which
    // version will carry them.
    let route_hops: &[[u8; SENDER_ID_LEN]] = if version >= 2 {
        packet.route.as_deref().unwrap_or(&[])
    } else {
        &[]
    };
    // Swift additionally rejects empty hops and normalises short/long ones to 8
    // bytes (`Data(hop.prefix(8))`, zero-padded). Our frozen `route` element type
    // is `[u8; 8]`, so both are structurally impossible here: a hop is always
    // exactly 8 bytes and can never be empty. The 255-hop cap is the only part of
    // that block that still has work to do.
    if route_hops.len() > MAX_ROUTE_HOPS {
        return Err(MeshError::OutOfRange("route exceeds 255 hops"));
    }

    let has_route = !route_hops.is_empty();
    let route_length = if has_route {
        1 + route_hops.len() * SENDER_ID_LEN
    } else {
        0
    };

    let original_size_field_bytes = if is_compressed { length_field_bytes } else { 0 };
    // `payloadLength` in the header covers the payload and, when compressed, the
    // original-size prefix — but NOT the route bytes.
    let payload_data_size = payload.len() + original_size_field_bytes;

    let max_payload_data_size = if version == 2 {
        u32::MAX as usize
    } else {
        u16::MAX as usize
    };
    if payload_data_size > max_payload_data_size {
        return Err(MeshError::LengthExceeded {
            declared: payload_data_size,
            max: max_payload_data_size,
        });
    }

    let recipient_length = match packet.recipient_id {
        Some(_) => RECIPIENT_ID_LEN,
        None => 0,
    };
    let signature_length = match packet.signature {
        Some(_) => SIGNATURE_LEN,
        None => 0,
    };
    // The trailing 255 mirrors Swift's `reserveCapacity`: headroom for the largest
    // padding block that can be appended.
    let mut data = Vec::with_capacity(
        header_size
            + SENDER_ID_LEN
            + recipient_length
            + route_length
            + payload_data_size
            + signature_length
            + 255,
    );

    data.push(version);
    data.push(packet.msg_type);
    data.push(packet.ttl);
    data.extend_from_slice(&packet.timestamp.to_be_bytes());

    let mut header_flags: u8 = 0;
    if packet.recipient_id.is_some() {
        header_flags |= flags::HAS_RECIPIENT;
    }
    if packet.signature.is_some() {
        header_flags |= flags::HAS_SIGNATURE;
    }
    if is_compressed {
        header_flags |= flags::IS_COMPRESSED;
    }
    // HAS_ROUTE is only valid for v2+; `route_hops` is already empty for v1.
    if has_route && version >= 2 {
        header_flags |= flags::HAS_ROUTE;
    }
    if packet.is_rsr {
        header_flags |= flags::IS_RSR;
    }
    data.push(header_flags);

    if version == 2 {
        data.extend_from_slice(&(payload_data_size as u32).to_be_bytes());
    } else {
        data.extend_from_slice(&(payload_data_size as u16).to_be_bytes());
    }

    // Sender and recipient are fixed 8-byte fields. Swift truncates or zero-pads
    // to reach that width; our frozen type is already `[u8; 8]`.
    data.extend_from_slice(&packet.sender_id);
    if let Some(recipient_id) = packet.recipient_id {
        data.extend_from_slice(&recipient_id);
    }

    if has_route {
        // Bounded by the 255-hop check above, so this cast cannot wrap.
        data.push(route_hops.len() as u8);
        for hop in route_hops {
            data.extend_from_slice(hop);
        }
    }

    if let Some(original_size) = original_payload_size {
        if version == 2 {
            data.extend_from_slice(&(original_size as u32).to_be_bytes());
        } else {
            data.extend_from_slice(&(original_size as u16).to_be_bytes());
        }
    }
    data.extend_from_slice(&payload);

    if let Some(signature) = packet.signature {
        data.extend_from_slice(&signature);
    }

    if padding {
        // Fully qualified: the `padding` parameter shadows the module in the value
        // namespace, and spelling out the path keeps that unambiguous to a reader.
        let optimal_size = crate::padding::optimal_block_size(data.len());
        return Ok(crate::padding::pad(&data, optimal_size));
    }
    Ok(data)
}

/// Parses a frame received from the radio.
///
/// The raw bytes are tried **first**; only if that fails are they unpadded and
/// tried again. That order is not an optimisation. Padding is unauthenticated and
/// PKCS#7 detection is ambiguous — a payload whose last byte is `0x01` is
/// indistinguishable from one pad byte — so stripping first would silently corrupt
/// legitimate frames. Parsing first works because the decoder tolerates unread
/// trailing bytes, which is exactly what padding is.
pub fn decode(bytes: &[u8]) -> Result<BitchatPacket, MeshError> {
    // Try as-is first (robust when padding was never applied).
    match decode_core(bytes) {
        Ok(packet) => Ok(packet),
        Err(raw_error) => {
            let unpadded = padding::unpad(bytes);
            // `unpad` returns the input untouched when it does not look padded;
            // re-parsing identical bytes would just reproduce `raw_error`.
            if unpadded.len() == bytes.len() {
                return Err(raw_error);
            }
            decode_core(&unpadded)
        }
    }
}

/// A bounds-checked forward cursor. Every read goes through here; nothing in this
/// module indexes a caller-supplied buffer directly.
struct Cursor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    /// Bytes left. `offset` is only ever advanced by a successful `take`, so it is
    /// always `<= data.len()`.
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.offset)
    }

    /// `needed` in the error is the *shortfall*, i.e. how many more bytes the frame
    /// would have had to carry.
    fn take(&mut self, n: usize) -> Result<&'a [u8], MeshError> {
        if n > self.remaining() {
            return Err(MeshError::Truncated {
                offset: self.offset,
                needed: n - self.remaining(),
            });
        }
        let end = self.offset + n;
        let slice = &self.data[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, MeshError> {
        let byte = self.take(1)?;
        // `take(1)` returned exactly one byte.
        match byte.first() {
            Some(&value) => Ok(value),
            None => Err(MeshError::Truncated {
                offset: self.offset,
                needed: 1,
            }),
        }
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], MeshError> {
        let slice = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn u16_be(&mut self) -> Result<u16, MeshError> {
        Ok(u16::from_be_bytes(self.array::<2>()?))
    }

    fn u32_be(&mut self) -> Result<u32, MeshError> {
        Ok(u32::from_be_bytes(self.array::<4>()?))
    }

    fn u64_be(&mut self) -> Result<u64, MeshError> {
        Ok(u64::from_be_bytes(self.array::<8>()?))
    }
}

fn decode_core(raw: &[u8]) -> Result<BitchatPacket, MeshError> {
    // Cheapest possible rejection: the smallest legal frame is a v1 header plus a
    // sender ID.
    let minimum = V1_HEADER_SIZE + SENDER_ID_LEN;
    if raw.len() < minimum {
        return Err(MeshError::Truncated {
            offset: raw.len(),
            needed: minimum - raw.len(),
        });
    }

    let mut cursor = Cursor::new(raw);

    let version = cursor.u8()?;
    let Some(header_size) = header_size(version) else {
        return Err(MeshError::UnsupportedVersion(version));
    };
    let length_field_bytes = length_field_size(version);
    let minimum_required = header_size + SENDER_ID_LEN;
    if raw.len() < minimum_required {
        return Err(MeshError::Truncated {
            offset: raw.len(),
            needed: minimum_required - raw.len(),
        });
    }

    let msg_type = cursor.u8()?;
    let ttl = cursor.u8()?;
    let timestamp = cursor.u64_be()?;

    let header_flags = cursor.u8()?;
    let has_recipient = header_flags & flags::HAS_RECIPIENT != 0;
    let has_signature = header_flags & flags::HAS_SIGNATURE != 0;
    let is_compressed = header_flags & flags::IS_COMPRESSED != 0;
    // HAS_ROUTE is v2-only. On a v1 frame the bit is IGNORED, never rejected: a
    // future peer setting a bit we do not implement must still be understood, and
    // honouring it here would misread the payload as a route.
    let has_route = version >= 2 && (header_flags & flags::HAS_ROUTE != 0);
    let is_rsr = header_flags & flags::IS_RSR != 0;

    let payload_length: usize = if version == 2 {
        cursor.u32_be()? as usize
    } else {
        cursor.u16_be()? as usize
    };

    // Anti-DoS: bound the declared length against a constant BEFORE anything is
    // allocated from it.
    if payload_length > MAX_FRAMED_FILE_BYTES {
        return Err(MeshError::LengthExceeded {
            declared: payload_length,
            max: MAX_FRAMED_FILE_BYTES,
        });
    }

    let sender_id = cursor.array::<SENDER_ID_LEN>()?;

    let recipient_id = if has_recipient {
        Some(cursor.array::<RECIPIENT_ID_LEN>()?)
    } else {
        None
    };

    // Route (v2 only). Route bytes are NOT counted in `payloadLength`, so the
    // cursor position after this block is the only thing that locates the payload.
    let mut route: Option<Vec<[u8; SENDER_ID_LEN]>> = None;
    if has_route {
        let route_count = usize::from(cursor.u8()?);
        if route_count > 0 {
            // `route_count <= 255`, and each hop is length-checked by `array`, so
            // this reserve is bounded at 2 KiB regardless of what the sender sent.
            let mut hops = Vec::with_capacity(route_count);
            for _ in 0..route_count {
                hops.push(cursor.array::<SENDER_ID_LEN>()?);
            }
            route = Some(hops);
        }
        // A count of zero decodes to `None`, matching the encoder: an empty route
        // never sets the flag, so this only happens on a hand-built frame.
    }

    let payload: Vec<u8> = if is_compressed {
        // `payloadLength` covers the original-size prefix as well as the deflate
        // stream, so it must be at least as long as the prefix.
        if payload_length < length_field_bytes {
            return Err(MeshError::OutOfRange(
                "compressed payload length is shorter than its original-size field",
            ));
        }
        let original_size: usize = if version == 2 {
            cursor.u32_be()? as usize
        } else {
            cursor.u16_be()? as usize
        };
        if original_size > MAX_FRAMED_FILE_BYTES {
            return Err(MeshError::LengthExceeded {
                declared: original_size,
                max: MAX_FRAMED_FILE_BYTES,
            });
        }
        let compressed_size = payload_length - length_field_bytes;
        if compressed_size == 0 {
            return Err(MeshError::OutOfRange("compressed payload is empty"));
        }
        let compressed = cursor.take(compressed_size)?;

        // Anti-DoS: a decompression bomb. Checked before we allocate the output
        // buffer, which is sized from `original_size`.
        let ratio = original_size as f64 / compressed_size as f64;
        if ratio > MAX_COMPRESSION_RATIO {
            return Err(MeshError::CompressionRatio { ratio: ratio as u64 });
        }

        let decompressed =
            compress::decompress(compressed, original_size).ok_or(MeshError::Decompression)?;
        // A truncated deflate stream inflates to fewer bytes than declared rather
        // than failing outright, so the length has to be checked explicitly.
        if decompressed.len() != original_size {
            return Err(MeshError::Decompression);
        }
        decompressed
    } else {
        cursor.take(payload_length)?.to_vec()
    };

    let signature = if has_signature {
        Some(cursor.array::<SIGNATURE_LEN>()?)
    } else {
        None
    };

    // No check that the cursor landed exactly on the end: bitchat's own guard is
    // `offset <= count`. Trailing bytes are legal and expected — they are the
    // padding, which is how a padded frame decodes on the first pass.
    Ok(BitchatPacket {
        version,
        msg_type,
        ttl,
        timestamp,
        sender_id,
        recipient_id,
        payload,
        signature,
        route,
        is_rsr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{BROADCAST_RECIPIENT, MessageType};

    fn sample_packet() -> BitchatPacket {
        BitchatPacket {
            version: 1,
            msg_type: MessageType::Message.as_u8(),
            ttl: 7,
            timestamp: 1_720_000_000_000,
            sender_id: [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77],
            recipient_id: None,
            payload: b"hello mesh".to_vec(),
            signature: None,
            route: None,
            is_rsr: false,
        }
    }

    /// Something deflate can shrink, so the compressed path is exercised.
    fn compressible(len: usize) -> Vec<u8> {
        b"compress-me "
            .iter()
            .copied()
            .cycle()
            .take(len)
            .collect::<Vec<u8>>()
    }

    /// A byte run using all 256 values: `should_compress` refuses it (ratio 1.0),
    /// so the payload rides uncompressed at exactly the length given.
    fn incompressible(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 256) as u8).collect()
    }

    /// Builds a frame by hand so the decoder can be fed shapes the encoder will
    /// never produce.
    struct FrameBuilder {
        bytes: Vec<u8>,
    }

    impl FrameBuilder {
        /// The sender ID is `01..08` rather than a constant run so that a frame
        /// with nothing appended does not end in a byte that `unpad` would read as
        /// valid PKCS#7 — otherwise the fallback path would rewrite these fixtures
        /// out from under the assertion.
        const SENDER: [u8; SENDER_ID_LEN] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];

        fn v1(header_flags: u8, payload_length: u16) -> Self {
            let mut bytes = vec![1u8, 0x02, 7];
            bytes.extend_from_slice(&0u64.to_be_bytes());
            bytes.push(header_flags);
            bytes.extend_from_slice(&payload_length.to_be_bytes());
            bytes.extend_from_slice(&Self::SENDER);
            Self { bytes }
        }

        fn v2(header_flags: u8, payload_length: u32) -> Self {
            let mut bytes = vec![2u8, 0x02, 7];
            bytes.extend_from_slice(&0u64.to_be_bytes());
            bytes.push(header_flags);
            bytes.extend_from_slice(&payload_length.to_be_bytes());
            bytes.extend_from_slice(&Self::SENDER);
            Self { bytes }
        }

        fn push(mut self, extra: &[u8]) -> Self {
            self.bytes.extend_from_slice(extra);
            self
        }

        fn build(self) -> Vec<u8> {
            self.bytes
        }
    }

    // --- round trips ---------------------------------------------------------

    #[test]
    fn v1_round_trips_unpadded_and_padded() {
        let packet = sample_packet();

        let unpadded = encode(&packet, false).expect("encode");
        assert_eq!(unpadded.len(), V1_HEADER_SIZE + SENDER_ID_LEN + 10);
        assert_eq!(decode(&unpadded).expect("decode"), packet);

        let padded = encode(&packet, true).expect("encode");
        assert_eq!(padded.len(), 256, "small frames round up to the first block");
        assert_eq!(decode(&padded).expect("decode"), packet);
    }

    #[test]
    fn v1_round_trips_with_recipient_and_signature() {
        let packet = BitchatPacket {
            recipient_id: Some([0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
            signature: Some([0x5A; SIGNATURE_LEN]),
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");

        let header_flags = encoded[FLAGS_OFFSET];
        assert_eq!(header_flags & flags::HAS_RECIPIENT, flags::HAS_RECIPIENT);
        assert_eq!(header_flags & flags::HAS_SIGNATURE, flags::HAS_SIGNATURE);
        assert_eq!(
            encoded.len(),
            V1_HEADER_SIZE + SENDER_ID_LEN + RECIPIENT_ID_LEN + 10 + SIGNATURE_LEN
        );
        assert_eq!(decode(&encoded).expect("decode"), packet);
    }

    #[test]
    fn both_broadcast_encodings_survive_the_round_trip_as_themselves() {
        let broadcast = BitchatPacket {
            recipient_id: Some(BROADCAST_RECIPIENT),
            ..sample_packet()
        };
        let absent = sample_packet();

        let decoded_broadcast = decode(&encode(&broadcast, false).expect("encode")).expect("decode");
        let decoded_absent = decode(&encode(&absent, false).expect("encode")).expect("decode");

        assert_eq!(decoded_broadcast.recipient_id, Some(BROADCAST_RECIPIENT));
        assert_eq!(decoded_absent.recipient_id, None);
        // The recipient field itself round-trips distinctly — an absent
        // recipient must not be re-encoded as all-`0xFF`, or the packet would no
        // longer be byte-identical to what its originator signed. Both forms
        // nonetheless *mean* broadcast.
        assert!(decoded_broadcast.is_broadcast());
        assert!(decoded_absent.is_broadcast());
    }

    #[test]
    fn v2_round_trips_with_route() {
        let route = vec![
            [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
            [0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18],
            [0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28],
        ];
        let packet = BitchatPacket {
            version: 2,
            route: Some(route.clone()),
            recipient_id: Some([0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28]),
            ..sample_packet()
        };

        let encoded = encode(&packet, false).expect("encode");
        assert_eq!(encoded[FLAGS_OFFSET] & flags::HAS_ROUTE, flags::HAS_ROUTE);

        let decoded = decode(&encoded).expect("decode");
        assert_eq!(decoded, packet);
        assert_eq!(decoded.route.as_deref(), Some(&route[..]));
    }

    #[test]
    fn unknown_message_types_and_the_rsr_flag_round_trip() {
        // Relays must forward types they do not understand.
        let packet = BitchatPacket {
            msg_type: 0xF3,
            is_rsr: true,
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");
        assert_eq!(encoded[FLAGS_OFFSET] & flags::IS_RSR, flags::IS_RSR);

        let decoded = decode(&encoded).expect("decode");
        assert_eq!(decoded, packet);
        assert_eq!(decoded.message_type(), None);
    }

    #[test]
    fn empty_payload_round_trips() {
        let packet = BitchatPacket {
            payload: Vec::new(),
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");
        assert_eq!(decode(&encoded).expect("decode"), packet);
    }

    // --- big-endian ------------------------------------------------------------

    /// TRAP: every multi-byte integer is big-endian (network byte order).
    #[test]
    fn trap_all_multibyte_integers_are_big_endian() {
        // 308 == 0x0134 bytes, high-entropy so it rides uncompressed and the
        // declared length is exactly the payload length.
        let packet = BitchatPacket {
            timestamp: 0x0102_0304_0506_0708,
            version: 2,
            payload: incompressible(0x0134),
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");

        assert_eq!(
            &encoded[3..11],
            &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
            "timestamp is big-endian"
        );
        // v2 payload length: 0x0134 == 308 in four big-endian bytes.
        assert_eq!(&encoded[12..16], &[0x00, 0x00, 0x01, 0x34]);

        let v1 = BitchatPacket {
            version: 1,
            ..packet.clone()
        };
        let v1_encoded = encode(&v1, false).expect("encode");
        assert_eq!(&v1_encoded[12..14], &[0x01, 0x34]);
    }

    // --- the signing preimage is padded ----------------------------------------

    /// TRAP: `signing_preimage` re-encodes WITH padding. Signing the unpadded
    /// bytes verifies locally and fails against every real peer.
    #[test]
    fn trap_signing_preimage_is_the_padded_encoding() {
        let packet = BitchatPacket {
            ttl: 7,
            is_rsr: true,
            signature: Some([0x11; SIGNATURE_LEN]),
            ..sample_packet()
        };
        let preimage = packet.signing_preimage().expect("preimage");

        let unsigned = BitchatPacket {
            ttl: 0,
            is_rsr: false,
            signature: None,
            ..packet.clone()
        };
        assert_eq!(preimage, encode(&unsigned, true).expect("encode"));
        assert_eq!(preimage.len(), 256, "preimage is padded to a block size");
        assert_ne!(
            preimage,
            encode(&unsigned, false).expect("encode"),
            "the unpadded encoding is the wrong preimage"
        );

        // ttl, isRSR and the signature are all excluded, so a relayed copy signs
        // the same bytes.
        let relayed = BitchatPacket {
            ttl: 3,
            ..packet.clone()
        };
        assert_eq!(relayed.signing_preimage().expect("preimage"), preimage);
    }

    // --- payloadLength accounting ----------------------------------------------

    /// TRAP: `payloadLength` excludes the route bytes.
    #[test]
    fn trap_payload_length_excludes_route_bytes() {
        let route = vec![[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]];
        let payload = b"test-payload".to_vec();

        let v1 = BitchatPacket {
            version: 1,
            payload: payload.clone(),
            route: Some(route.clone()),
            ..sample_packet()
        };
        let v2 = BitchatPacket {
            version: 2,
            ..v1.clone()
        };

        let v1_encoded = encode(&v1, false).expect("encode");
        let v2_encoded = encode(&v2, false).expect("encode");

        // The header field is the bare payload length in both versions.
        assert_eq!(
            u16::from_be_bytes([v1_encoded[12], v1_encoded[13]]) as usize,
            payload.len()
        );
        assert_eq!(
            u32::from_be_bytes([
                v2_encoded[12],
                v2_encoded[13],
                v2_encoded[14],
                v2_encoded[15]
            ]) as usize,
            payload.len(),
            "route bytes are framed but not counted"
        );

        // 2 bytes of wider length field + 1 route count + 1 hop.
        assert_eq!(v2_encoded.len() - v1_encoded.len(), 2 + 1 + 8);
    }

    /// TRAP: `payloadLength` INCLUDES the 2-byte (v1) / 4-byte (v2) original
    /// size field when IS_COMPRESSED is set.
    #[test]
    fn trap_payload_length_includes_original_size_field_when_compressed() {
        for (version, header_len, field_len) in [(1u8, V1_HEADER_SIZE, 2usize), (2, V2_HEADER_SIZE, 4)]
        {
            let payload = compressible(2000);
            let packet = BitchatPacket {
                version,
                payload: payload.clone(),
                ..sample_packet()
            };
            let encoded = encode(&packet, false).expect("encode");
            assert_eq!(
                encoded[FLAGS_OFFSET] & flags::IS_COMPRESSED,
                flags::IS_COMPRESSED,
                "v{version} payload should have compressed"
            );

            let declared = if version == 2 {
                u32::from_be_bytes([encoded[12], encoded[13], encoded[14], encoded[15]]) as usize
            } else {
                u16::from_be_bytes([encoded[12], encoded[13]]) as usize
            };

            let body_len = encoded.len() - header_len - SENDER_ID_LEN;
            assert_eq!(
                declared, body_len,
                "declared length must cover original-size field + deflate stream"
            );
            assert!(declared > field_len);
            assert_eq!(decode(&encoded).expect("decode").payload, payload);
        }
    }

    // --- HAS_ROUTE is v2-only --------------------------------------------------

    /// TRAP: a v1 packet never encodes a route, even when one is attached.
    #[test]
    fn trap_v1_encode_ignores_an_attached_route() {
        let packet = BitchatPacket {
            version: 1,
            route: Some(vec![[0x01; SENDER_ID_LEN], [0x02; SENDER_ID_LEN]]),
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");

        assert_eq!(
            encoded[FLAGS_OFFSET] & flags::HAS_ROUTE,
            0,
            "v1 must not set HAS_ROUTE"
        );
        assert_eq!(encoded.len(), V1_HEADER_SIZE + SENDER_ID_LEN + 10);

        let decoded = decode(&encoded).expect("decode");
        assert_eq!(decoded.route, None);
        assert_eq!(decoded.payload, packet.payload);
    }

    /// TRAP: a v1 frame from a peer that DOES set the bit must have it ignored,
    /// not rejected — and the bytes after the sender ID are payload, not a route.
    #[test]
    fn trap_v1_decode_ignores_the_has_route_flag_instead_of_rejecting() {
        let frame = FrameBuilder::v1(flags::HAS_ROUTE, 4)
            .push(&[0xDE, 0xAD, 0xBE, 0xEF])
            .build();

        let decoded = decode(&frame).expect("v1 frame with HAS_ROUTE must still decode");
        assert_eq!(decoded.route, None);
        assert_eq!(
            decoded.payload,
            vec![0xDE, 0xAD, 0xBE, 0xEF],
            "the route bit must not consume payload bytes on v1"
        );
    }

    #[test]
    fn v2_route_count_of_zero_decodes_as_no_route() {
        let frame = FrameBuilder::v2(flags::HAS_ROUTE, 2)
            .push(&[0x00]) // route count
            .push(&[0xAA, 0xBB])
            .build();
        let decoded = decode(&frame).expect("decode");
        assert_eq!(decoded.route, None);
        assert_eq!(decoded.payload, vec![0xAA, 0xBB]);
    }

    // --- fixed-width IDs and the hop cap ---------------------------------------

    /// TRAP: at most 255 hops — the count is one byte.
    #[test]
    fn trap_route_longer_than_255_hops_is_rejected() {
        let packet = BitchatPacket {
            version: 2,
            route: Some(vec![[0x09; SENDER_ID_LEN]; 256]),
            ..sample_packet()
        };
        assert_eq!(
            encode(&packet, false),
            Err(MeshError::OutOfRange("route exceeds 255 hops"))
        );

        // Exactly 255 is legal and round-trips.
        let at_cap = BitchatPacket {
            version: 2,
            route: Some(vec![[0x09; SENDER_ID_LEN]; 255]),
            ..sample_packet()
        };
        let encoded = encode(&at_cap, false).expect("encode");
        assert_eq!(encoded[V2_HEADER_SIZE + SENDER_ID_LEN], 255);
        assert_eq!(decode(&encoded).expect("decode"), at_cap);
    }

    /// TRAP: sender and recipient are fixed 8-byte fields. Our frozen types are
    /// `[u8; 8]`, so short IDs are zero-padded by construction — a `[0xAB, 0, ...]`
    /// sender is exactly what a one-byte ID looks like on the wire.
    #[test]
    fn trap_ids_are_fixed_eight_byte_fields() {
        let packet = BitchatPacket {
            sender_id: [0xAB, 0, 0, 0, 0, 0, 0, 0],
            recipient_id: Some([0xCD, 0, 0, 0, 0, 0, 0, 0]),
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");
        assert_eq!(
            &encoded[V1_HEADER_SIZE..V1_HEADER_SIZE + SENDER_ID_LEN],
            &[0xAB, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(decode(&encoded).expect("decode"), packet);
    }

    // --- decode order ----------------------------------------------------------

    /// TRAP: the raw bytes are parsed first, the unpadded bytes only on failure.
    #[test]
    fn trap_decode_tries_raw_bytes_before_unpadding() {
        // A payload that ends in 0x01 looks like one byte of PKCS#7 padding.
        // Unpadding first would eat the last payload byte.
        let packet = BitchatPacket {
            payload: vec![0xFE, 0xFE, 0x01],
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");
        assert_eq!(padding::unpad(&encoded).len(), encoded.len() - 1);
        assert_eq!(
            decode(&encoded).expect("decode").payload,
            vec![0xFE, 0xFE, 0x01],
            "raw-first parsing is what keeps this payload intact"
        );

        // A padded frame still decodes on the first pass: the pad bytes sit past
        // the payload and are simply never read.
        let padded = encode(&packet, true).expect("encode");
        assert_eq!(padded.len(), 256);
        assert_eq!(decode(&padded).expect("decode"), packet);

        // When the raw parse fails and unpadding changes nothing, the original
        // error is what surfaces.
        let junk = vec![0xFFu8; 30];
        assert_eq!(padding::unpad(&junk).len(), junk.len());
        assert_eq!(decode(&junk), Err(MeshError::UnsupportedVersion(0xFF)));
    }

    #[test]
    fn decode_falls_back_to_unpadding_when_the_raw_parse_fails() {
        // Frame declares 4 payload bytes but carries 3, followed by a valid
        // one-byte PKCS#7 pad that completes it once stripped... which it cannot,
        // since stripping only shortens. Assert the fallback runs and still errs,
        // rather than panicking or looping.
        let frame = FrameBuilder::v1(0, 4).push(&[0xAA, 0xBB, 0x01]).build();
        assert!(matches!(
            decode(&frame),
            Err(MeshError::Truncated { .. })
        ));
    }

    // --- compression ---------------------------------------------------------

    #[test]
    fn compressed_payload_round_trips_on_both_versions() {
        for version in [1u8, 2] {
            let payload = compressible(4000);
            let packet = BitchatPacket {
                version,
                payload: payload.clone(),
                ..sample_packet()
            };
            let encoded = encode(&packet, false).expect("encode");
            assert!(
                encoded.len() < payload.len(),
                "v{version} frame should be smaller than its payload"
            );
            assert_eq!(decode(&encoded).expect("decode"), packet);
        }
    }

    #[test]
    fn compressed_payload_round_trips_alongside_a_route() {
        let payload = compressible(1650);
        let route = vec![[0x01; SENDER_ID_LEN], [0x02; SENDER_ID_LEN]];
        let packet = BitchatPacket {
            version: 2,
            payload: payload.clone(),
            route: Some(route),
            recipient_id: Some([0x02; SENDER_ID_LEN]),
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");
        assert_eq!(
            encoded[FLAGS_OFFSET] & (flags::IS_COMPRESSED | flags::HAS_ROUTE),
            flags::IS_COMPRESSED | flags::HAS_ROUTE
        );
        assert_eq!(decode(&encoded).expect("decode"), packet);
    }

    #[test]
    fn high_entropy_payload_is_not_compressed() {
        let packet = BitchatPacket {
            payload: incompressible(1000),
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");
        assert_eq!(encoded[FLAGS_OFFSET] & flags::IS_COMPRESSED, 0);
        assert_eq!(encoded.len(), V1_HEADER_SIZE + SENDER_ID_LEN + 1000);
    }

    #[test]
    fn v1_declines_compression_when_the_original_size_does_not_fit_the_field() {
        // Compressible, but 70_000 bytes cannot be described by a 16-bit original
        // size — so compression is skipped, and then the frame itself is too big
        // for a 16-bit payloadLength.
        let packet = BitchatPacket {
            version: 1,
            payload: compressible(70_000),
            ..sample_packet()
        };
        assert_eq!(
            encode(&packet, false),
            Err(MeshError::LengthExceeded {
                declared: 70_000,
                max: u16::MAX as usize,
            })
        );

        // The same payload is fine on v2, where both fields are 32-bit.
        let v2 = BitchatPacket {
            version: 2,
            ..packet.clone()
        };
        let encoded = encode(&v2, false).expect("encode");
        assert_eq!(decode(&encoded).expect("decode"), v2);
    }

    #[test]
    fn v1_rejects_an_uncompressible_payload_over_the_16_bit_length_field() {
        let packet = BitchatPacket {
            version: 1,
            payload: incompressible(70_000),
            ..sample_packet()
        };
        assert!(matches!(
            encode(&packet, false),
            Err(MeshError::LengthExceeded { .. })
        ));
    }

    // --- anti-DoS --------------------------------------------------------------

    /// TRAP: an oversized `payloadLength` is refused before any allocation.
    #[test]
    fn trap_decoder_rejects_payload_length_over_the_framed_file_cap() {
        assert_eq!(MAX_FRAMED_FILE_BYTES, 1_179_760);

        let over = (MAX_FRAMED_FILE_BYTES + 1) as u32;
        let frame = FrameBuilder::v2(0, over).push(&[0x01, 0x02, 0x03]).build();
        assert_eq!(
            decode(&frame),
            Err(MeshError::LengthExceeded {
                declared: MAX_FRAMED_FILE_BYTES + 1,
                max: MAX_FRAMED_FILE_BYTES,
            })
        );

        // Under the cap but unbacked by bytes: truncation, not a huge allocation.
        let frame = FrameBuilder::v1(0, u16::MAX).push(&[0x01, 0x02, 0x03]).build();
        assert!(matches!(decode(&frame), Err(MeshError::Truncated { .. })));
    }

    /// TRAP: a decompression bomb is refused on the declared ratio alone,
    /// before the output buffer is allocated.
    #[test]
    fn trap_decoder_rejects_implausible_compression_ratios() {
        // v1: payloadLength 3 == 2-byte original size + 1 byte of "deflate".
        // 65535 / 1 == 65535 : 1, far over the 50_000 : 1 ceiling.
        let frame = FrameBuilder::v1(flags::IS_COMPRESSED, 3)
            .push(&u16::MAX.to_be_bytes())
            .push(&[0x99])
            .build();
        assert_eq!(
            decode(&frame),
            Err(MeshError::CompressionRatio { ratio: 65_535 })
        );

        // Just under the ceiling the ratio check passes and the failure comes from
        // the deflate stream itself, proving the guard is on the ratio only.
        let frame = FrameBuilder::v1(flags::IS_COMPRESSED, 3)
            .push(&50_000u16.to_be_bytes())
            .push(&[0x99])
            .build();
        assert_eq!(decode(&frame), Err(MeshError::Decompression));
    }

    #[test]
    fn decoder_rejects_a_compressed_original_size_over_the_cap() {
        let original = (MAX_FRAMED_FILE_BYTES + 1) as u32;
        let frame = FrameBuilder::v2(flags::IS_COMPRESSED, 8)
            .push(&original.to_be_bytes())
            .push(&[0x11, 0x22, 0x33, 0x44])
            .build();
        assert_eq!(
            decode(&frame),
            Err(MeshError::LengthExceeded {
                declared: MAX_FRAMED_FILE_BYTES + 1,
                max: MAX_FRAMED_FILE_BYTES,
            })
        );
    }

    #[test]
    fn decoder_rejects_compressed_frames_with_no_room_for_the_original_size() {
        // payloadLength 1 < the 2-byte v1 original-size field.
        let frame = FrameBuilder::v1(flags::IS_COMPRESSED, 1).push(&[0x99]).build();
        assert_eq!(
            decode(&frame),
            Err(MeshError::OutOfRange(
                "compressed payload length is shorter than its original-size field"
            ))
        );

        // payloadLength 3 < the 4-byte v2 field.
        let frame = FrameBuilder::v2(flags::IS_COMPRESSED, 3)
            .push(&[0x99, 0x98, 0x97])
            .build();
        assert!(matches!(decode(&frame), Err(MeshError::OutOfRange(_))));
    }

    #[test]
    fn decoder_rejects_a_compressed_frame_with_an_empty_deflate_stream() {
        let frame = FrameBuilder::v1(flags::IS_COMPRESSED, 2)
            .push(&[0x00, 0x10])
            .build();
        assert_eq!(
            decode(&frame),
            Err(MeshError::OutOfRange("compressed payload is empty"))
        );
    }

    #[test]
    fn decoder_rejects_a_deflate_stream_that_inflates_to_the_wrong_length() {
        let payload = compressible(500);
        let packet = BitchatPacket {
            payload: payload.clone(),
            ..sample_packet()
        };
        let mut encoded = encode(&packet, false).expect("encode");
        assert_eq!(encoded[FLAGS_OFFSET] & flags::IS_COMPRESSED, flags::IS_COMPRESSED);

        // Overstate the original size by one byte, keeping the ratio plausible.
        //
        // Overstating, not understating: an *under*stated size is not detectable
        // here, because the inflate output buffer is sized from the claim, so the
        // stream is simply cut short at exactly the claimed length and the
        // equality check passes. bitchat has the same blind spot — the payload it
        // hands up is then a prefix of what the sender compressed, and only the
        // signature catches it.
        let field = V1_HEADER_SIZE + SENDER_ID_LEN;
        let claimed = (payload.len() + 1) as u16;
        encoded[field] = claimed.to_be_bytes()[0];
        encoded[field + 1] = claimed.to_be_bytes()[1];
        // A trailing 0x00 is an ignored byte on the raw pass and makes `unpad` a
        // no-op, so this asserts against the raw parse and not the fallback.
        encoded.push(0x00);
        assert_eq!(decode(&encoded), Err(MeshError::Decompression));
    }

    #[test]
    fn decoder_rejects_garbage_in_place_of_a_deflate_stream() {
        let frame = FrameBuilder::v1(flags::IS_COMPRESSED, 6)
            .push(&64u16.to_be_bytes())
            .push(&[0xFF, 0xFF, 0xFF, 0xFF])
            .build();
        assert_eq!(decode(&frame), Err(MeshError::Decompression));
    }

    // --- malformed and truncated input ---------------------------------------

    #[test]
    fn decoder_rejects_unsupported_versions() {
        for version in [0u8, 3, 0x7F, 0xFF] {
            let mut frame = FrameBuilder::v1(0, 0).build();
            frame[0] = version;
            assert_eq!(decode(&frame), Err(MeshError::UnsupportedVersion(version)));
        }
    }

    #[test]
    fn decoder_rejects_input_shorter_than_the_minimum_frame() {
        for len in 0..(V1_HEADER_SIZE + SENDER_ID_LEN) {
            let frame = vec![1u8; len];
            assert!(
                matches!(decode(&frame), Err(MeshError::Truncated { .. })),
                "len {len} must be rejected as truncated"
            );
        }
    }

    #[test]
    fn v2_frame_shorter_than_the_v2_minimum_is_truncated_not_misparsed() {
        // 22 bytes clears the v1 minimum but not the v2 one (24).
        let mut frame = vec![2u8; V1_HEADER_SIZE + SENDER_ID_LEN];
        frame[0] = 2;
        assert!(matches!(decode(&frame), Err(MeshError::Truncated { .. })));
    }

    /// Truncation at every byte boundary of a fully populated frame.
    #[test]
    fn every_truncation_of_a_valid_frame_is_rejected_without_panicking() {
        let packet = BitchatPacket {
            version: 2,
            recipient_id: Some([0x88; SENDER_ID_LEN]),
            signature: Some([0x5A; SIGNATURE_LEN]),
            route: Some(vec![[0x01; SENDER_ID_LEN], [0x02; SENDER_ID_LEN]]),
            payload: compressible(1200),
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");
        assert_eq!(decode(&encoded).expect("decode"), packet);

        for cut in 0..encoded.len() {
            let truncated = &encoded[..cut];
            assert!(
                decode(truncated).is_err(),
                "truncating to {cut} of {} bytes must fail cleanly",
                encoded.len()
            );
        }
    }

    #[test]
    fn every_truncation_of_an_uncompressed_signed_frame_is_rejected() {
        let packet = BitchatPacket {
            recipient_id: Some([0x88; SENDER_ID_LEN]),
            signature: Some([0x5A; SIGNATURE_LEN]),
            payload: incompressible(300),
            ..sample_packet()
        };
        let encoded = encode(&packet, false).expect("encode");
        for cut in 0..encoded.len() {
            assert!(decode(&encoded[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn decoder_rejects_a_missing_signature() {
        let frame = FrameBuilder::v1(flags::HAS_SIGNATURE, 2)
            .push(&[0xAA, 0xBB])
            .push(&[0x01; 63]) // one byte short of a signature
            .build();
        assert!(matches!(decode(&frame), Err(MeshError::Truncated { .. })));
    }

    #[test]
    fn decoder_rejects_a_missing_recipient() {
        let frame = FrameBuilder::v1(flags::HAS_RECIPIENT, 0)
            .push(&[0x01; 7])
            .build();
        assert!(matches!(decode(&frame), Err(MeshError::Truncated { .. })));
    }

    #[test]
    fn decoder_rejects_a_route_that_runs_off_the_end() {
        let frame = FrameBuilder::v2(flags::HAS_ROUTE, 0)
            .push(&[0x04]) // claims four hops
            .push(&[0x01; SENDER_ID_LEN])
            .build();
        assert!(matches!(decode(&frame), Err(MeshError::Truncated { .. })));
    }

    #[test]
    fn decoder_tolerates_trailing_bytes_past_the_frame() {
        let packet = sample_packet();
        let mut encoded = encode(&packet, false).expect("encode");
        encoded.extend_from_slice(&[0x00; 40]);
        assert_eq!(decode(&encoded).expect("decode"), packet);
    }

    #[test]
    fn encoder_rejects_unsupported_versions() {
        for version in [0u8, 3, 0xFF] {
            let packet = BitchatPacket {
                version,
                ..sample_packet()
            };
            assert_eq!(
                encode(&packet, false),
                Err(MeshError::UnsupportedVersion(version))
            );
        }
    }

    #[test]
    fn padded_frames_land_on_block_sizes() {
        for payload_len in [0usize, 10, 200, 400, 900, 1900] {
            let packet = BitchatPacket {
                payload: incompressible(payload_len),
                ..sample_packet()
            };
            let encoded = encode(&packet, true).expect("encode");
            if encoded.len() <= 2048 {
                assert!(
                    padding::BLOCK_SIZES.contains(&encoded.len()),
                    "payload {payload_len} produced non-block size {}",
                    encoded.len()
                );
            }
            assert_eq!(decode(&encoded).expect("decode").payload.len(), payload_len);
        }
    }

    #[test]
    fn very_large_frames_are_emitted_unpadded() {
        let packet = BitchatPacket {
            version: 2,
            payload: incompressible(4000),
            ..sample_packet()
        };
        let padded = encode(&packet, true).expect("encode");
        let unpadded = encode(&packet, false).expect("encode");
        assert_eq!(
            padded, unpadded,
            "above 2048 bytes optimal_block_size returns the raw length"
        );
        assert_eq!(decode(&padded).expect("decode"), packet);
    }

    #[test]
    fn header_sizes_and_flag_bits_match_the_swift_constants() {
        assert_eq!(V1_HEADER_SIZE, 14);
        assert_eq!(V2_HEADER_SIZE, 16);
        assert_eq!(FLAGS_OFFSET, 11);
        assert_eq!(flags::HAS_RECIPIENT, 0x01);
        assert_eq!(flags::HAS_SIGNATURE, 0x02);
        assert_eq!(flags::IS_COMPRESSED, 0x04);
        assert_eq!(flags::HAS_ROUTE, 0x08);
        assert_eq!(flags::IS_RSR, 0x10);
    }
}
