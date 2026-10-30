use crate::compress;
use crate::error::MeshError;
use crate::packet::{BitchatPacket, SENDER_ID_LEN, SIGNATURE_LEN};
use crate::padding;

use super::{
    MAX_COMPRESSION_RATIO, MAX_FRAMED_FILE_BYTES, RECIPIENT_ID_LEN, V1_HEADER_SIZE, flags,
    header_size, length_field_size,
};

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
            return Err(MeshError::CompressionRatio {
                ratio: ratio as u64,
            });
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
