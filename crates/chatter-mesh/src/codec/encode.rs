use std::borrow::Cow;

use crate::compress;
use crate::error::MeshError;
use crate::packet::{BitchatPacket, SENDER_ID_LEN, SIGNATURE_LEN};

use super::{MAX_ROUTE_HOPS, RECIPIENT_ID_LEN, flags, header_size, length_field_size};

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
