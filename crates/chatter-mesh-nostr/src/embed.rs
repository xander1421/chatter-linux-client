//! `bitchat1:` packet embedding — a whole mesh packet carried inside a Nostr DM.
//!
//! Ported from `bitchat/Nostr/NostrEmbeddedBitChat.swift` and the decode half of
//! `ViewModels/NostrInboundPipeline.swift`, with the padding rules of
//! `Nostr/Base64URLCoding.swift`.
//!
//! The construction is deliberately dull: `"bitchat1:" + base64url(packet)`, where
//! `packet` is the ordinary padded wire encoding a Bluetooth peer would have seen.
//! That is the whole point of the bridge — the same [`BitchatPacket`] that travels
//! over BLE travels over a relay, so the layer above (receipts, TLV bodies, message
//! IDs) is written once and does not know which transport carried it.
//!
//! # What the embedded packet is *not*
//!
//! The `NoiseEncrypted` (0x11) type tag is a lie of convenience inherited from
//! bitchat: nothing here is Noise-encrypted. The payload is a cleartext
//! [`NoisePayloadType`] byte followed by a TLV body, and the actual confidentiality
//! comes from the gift wrap around the DM. The tag is what makes a mesh peer's
//! existing payload dispatch accept the packet unchanged, so it stays.
//!
//! This module is pure and has no feature gate: it is the bridge between
//! `chatter-mesh` and the Nostr event layer, and both halves of that bridge must
//! build without tokio.

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

use chatter_mesh::codec::{self, MAX_FRAMED_FILE_BYTES};
use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::private_message::PrivateMessagePacket;
use chatter_mesh::{
    BitchatPacket, MessageType, NoisePayloadType, SENDER_ID_LEN, encode_noise_payload,
};

use crate::EMBEDDED_PACKET_PREFIX;
use crate::error::NostrError;

/// Hop limit stamped on every embedded packet.
///
/// A packet arriving over Nostr has not spent any of its budget on radio hops, so
/// it enters the mesh with the full default TTL — the receiving client may relay it
/// onward to peers that have no internet path.
pub const EMBEDDED_TTL: u8 = 7;

/// Encoder: URL-safe alphabet, **no** padding.
///
/// `Base64URLCoding.encode` strips every `=`, so a padded encoding would produce a
/// different string for the same packet and break nothing visibly — until a peer
/// compares content hashes. Match it exactly.
const B64_ENCODE: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_encode_padding(false),
);

/// Decoder: URL-safe alphabet, padding optional.
///
/// `Base64URLCoding.decode` *re-adds* the padding it needs before decoding, so it
/// accepts both padded and unpadded input. `Indifferent` is the same contract.
/// Trailing non-zero bits stay rejected: that input is malformed either way, and a
/// relay is not trusted.
const B64_DECODE: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// `"bitchat1:" + base64url(packet.to_binary_data())`.
///
/// The packet is encoded **padded**, because `BitchatPacket.toBinaryData()` defaults
/// to `padding: true` and that default is what every real peer emits.
pub fn encode_embedded(packet: &BitchatPacket) -> Result<String, NostrError> {
    let bytes = codec::encode(packet, true)?;
    let mut out = String::with_capacity(EMBEDDED_PACKET_PREFIX.len() + bytes.len().div_ceil(3) * 4);
    out.push_str(EMBEDDED_PACKET_PREFIX);
    B64_ENCODE.encode_string(&bytes, &mut out);
    Ok(out)
}

/// Parses a `bitchat1:` DM body back into a packet.
///
/// Both length checks are load-bearing and both come before any allocation sized
/// from the input: the encoded-length check bounds the base64 decode, and the
/// decoded-length check bounds what reaches [`codec::decode`]. A relay can hand us
/// any string it likes.
pub fn decode_embedded(content: &str) -> Result<BitchatPacket, NostrError> {
    let encoded = content
        .strip_prefix(EMBEDDED_PACKET_PREFIX)
        .ok_or(NostrError::InvalidEvent(
            "content is not a bitchat1: payload",
        ))?;

    // `((maxBytes + 2) / 3) * 4` — the padded base64 length of the largest frame we
    // would accept. Unpadded input is shorter, so this is a ceiling for both forms.
    let max_encoded = MAX_FRAMED_FILE_BYTES.div_ceil(3) * 4;
    if encoded.len() > max_encoded {
        return Err(NostrError::InvalidEvent(
            "embedded packet exceeds the maximum frame size",
        ));
    }

    let bytes = B64_DECODE
        .decode(encoded)
        .map_err(|_| NostrError::InvalidEvent("embedded packet is not valid base64url"))?;
    if bytes.len() > MAX_FRAMED_FILE_BYTES {
        return Err(NostrError::InvalidEvent(
            "embedded packet exceeds the maximum frame size",
        ));
    }

    Ok(codec::decode(&bytes)?)
}

/// Whether a decrypted DM body carries an embedded packet.
pub fn is_embedded(content: &str) -> bool {
    content.starts_with(EMBEDDED_PACKET_PREFIX)
}

/// Builds the `noiseEncrypted` packet carrying a private message, per
/// `NostrEmbeddedBitChat.encodePMForNostr`.
///
/// `recipient` is normalised first — see [`normalize_recipient_peer_id`]. A
/// recipient with no 8-byte wire form is encoded as *absent* rather than as an
/// error, which is what Swift's `Data(hexString:)` returning `nil` amounts to.
pub fn embed_private_message(
    content: &str,
    message_id: &str,
    recipient: &PeerId,
    sender: &PeerId,
    timestamp_ms: u64,
) -> Result<String, NostrError> {
    let payload = private_message_payload(content, message_id)?;
    embed_packet(payload, Some(recipient), sender, timestamp_ms)
}

/// The same private message with no embedded recipient, per
/// `encodePMForNostrNoRecipient`.
///
/// Used for geohash DMs, where the two parties know each other only by ephemeral
/// Nostr pubkey: there is no stable mesh peer ID to name, and writing a wrong one
/// would be worse than writing none. The gift wrap already addresses the recipient.
pub fn embed_private_message_no_recipient(
    content: &str,
    message_id: &str,
    sender: &PeerId,
    timestamp_ms: u64,
) -> Result<String, NostrError> {
    let payload = private_message_payload(content, message_id)?;
    embed_packet(payload, None, sender, timestamp_ms)
}

/// Delivered / read acks, per `encodeAckForNostr` and
/// `encodeAckForNostrNoRecipient`.
///
/// `recipient: None` selects the no-recipient variant used for geohash DMs.
///
/// The ack body is the message ID as raw UTF-8 — **not** a TLV. That asymmetry with
/// the private-message payload is bitchat's, and `NoisePayload.decode` on the far
/// side reads it back the same way.
pub fn embed_ack(
    payload_type: NoisePayloadType,
    message_id: &str,
    recipient: Option<&PeerId>,
    sender: &PeerId,
    timestamp_ms: u64,
) -> Result<String, NostrError> {
    if !matches!(
        payload_type,
        NoisePayloadType::Delivered | NoisePayloadType::ReadReceipt
    ) {
        return Err(NostrError::InvalidEvent(
            "ack payload type must be delivered or read receipt",
        ));
    }
    let payload = encode_noise_payload(payload_type, message_id.as_bytes());
    embed_packet(payload, recipient, sender, timestamp_ms)
}

/// Reproduces `NostrEmbeddedBitChat.normalizeRecipientPeerID`.
///
/// A 32-byte hex value is a Noise static public key, and the wire wants the peer ID
/// *derived* from it — `SHA256(key)[..8]`, never the truncated key itself. An
/// 8-byte value is already a routing ID. Anything else is handed back untouched and
/// will simply fail to produce a recipient field.
pub fn normalize_recipient_peer_id(recipient: &PeerId) -> PeerId {
    match hex_to_bytes(recipient.as_str()) {
        Some(bytes) if bytes.len() == 32 => PeerId::from_noise_key(&bytes),
        _ => recipient.clone(),
    }
}

fn private_message_payload(content: &str, message_id: &str) -> Result<Vec<u8>, NostrError> {
    let tlv = PrivateMessagePacket::new(message_id.to_string(), content.to_string()).encode()?;
    Ok(encode_noise_payload(NoisePayloadType::PrivateMessage, &tlv))
}

fn embed_packet(
    payload: Vec<u8>,
    recipient: Option<&PeerId>,
    sender: &PeerId,
    timestamp_ms: u64,
) -> Result<String, NostrError> {
    // `Data(hexString: senderPeerID.id) ?? Data()` — a sender ID that is not hex
    // becomes empty, and `BinaryProtocol.encode` zero-pads it to eight bytes.
    let sender_id = wire_id(sender).unwrap_or([0u8; SENDER_ID_LEN]);
    let recipient_id = recipient.and_then(|peer| wire_id(&normalize_recipient_peer_id(peer)));

    let packet = BitchatPacket::new(
        MessageType::NoiseEncrypted,
        sender_id,
        recipient_id,
        timestamp_ms,
        payload,
        EMBEDDED_TTL,
    );
    encode_embedded(&packet)
}

/// The 8-byte wire form of a peer ID, as `BinaryProtocol.encode` would write it:
/// hex-decode the whole ID string, take the first eight bytes, zero-pad.
///
/// Note this is *not* `PeerId::to_routing_bytes`, which also tries the bare part
/// and the fingerprint form. Swift's embedding path only ever hex-decodes the full
/// `id`, so a prefixed ID (`nostr_…`) yields no recipient at all — and reproducing
/// that is the difference between "no recipient field" and "a recipient field
/// containing the first eight bytes of something else".
fn wire_id(peer: &PeerId) -> Option<[u8; SENDER_ID_LEN]> {
    let bytes = hex_to_bytes(peer.as_str())?;
    let mut out = [0u8; SENDER_ID_LEN];
    let take = bytes.len().min(SENDER_ID_LEN);
    out.get_mut(..take)?.copy_from_slice(bytes.get(..take)?);
    Some(out)
}

/// Mirrors Swift's `Data(hexString:)`: surrounding whitespace trimmed, an optional
/// `0x`/`0X` prefix, an even number of ASCII hex digits. Anything else is `None`.
///
/// An empty string decodes to an empty vector there, and does here — which is why
/// [`wire_id`] pads rather than requiring eight bytes.
fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let trimmed = s.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);

    let bytes = body.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }

    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let (Some(hi), Some(lo)) = (pair.first(), pair.get(1)) else {
            return None;
        };
        // Every byte of a multi-byte UTF-8 character is >= 0x80, so working on
        // bytes cannot split a character into something that decodes.
        let hi = char::from(*hi).to_digit(16)?;
        let lo = char::from(*lo).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh::decode_noise_payload;

    const SENDER_HEX: &str = "fedcba9876543210";
    const RECIPIENT_HEX: &str = "0011223344556677";

    fn sender() -> PeerId {
        PeerId::from_str(SENDER_HEX)
    }

    fn recipient() -> PeerId {
        PeerId::from_str(RECIPIENT_HEX)
    }

    #[test]
    fn encode_embedded_uses_the_frozen_prefix_and_unpadded_base64url() {
        let packet = BitchatPacket::new(
            MessageType::NoiseEncrypted,
            [0xAB; SENDER_ID_LEN],
            None,
            1_700_000_000_000,
            vec![0x01, 0x02, 0x03],
            EMBEDDED_TTL,
        );
        let encoded = encode_embedded(&packet).expect("encodes");

        assert!(encoded.starts_with("bitchat1:"));
        assert!(is_embedded(&encoded));
        let body = encoded.strip_prefix("bitchat1:").expect("prefix present");
        assert!(!body.contains('='), "padding must be stripped: {body}");
        assert!(
            !body.contains('+') && !body.contains('/'),
            "url-safe alphabet"
        );
    }

    #[test]
    fn round_trips_through_decode() {
        let packet = BitchatPacket::new(
            MessageType::NoiseEncrypted,
            [0x11; SENDER_ID_LEN],
            Some([0x22; SENDER_ID_LEN]),
            42,
            b"a payload".to_vec(),
            EMBEDDED_TTL,
        );
        let decoded =
            decode_embedded(&encode_embedded(&packet).expect("encodes")).expect("decodes");
        assert_eq!(decoded, packet);
    }

    #[test]
    fn encoding_is_deterministic() {
        let packet = BitchatPacket::new(
            MessageType::NoiseEncrypted,
            [0x11; SENDER_ID_LEN],
            None,
            7,
            b"same".to_vec(),
            EMBEDDED_TTL,
        );
        assert_eq!(
            encode_embedded(&packet).expect("encodes"),
            encode_embedded(&packet).expect("encodes")
        );
    }

    #[test]
    fn decode_accepts_padded_input_too() {
        // A peer that used a padding-emitting encoder must still be understood:
        // `Base64URLCoding.decode` re-adds padding before decoding, so it never
        // rejects input for having some already.
        let packet = BitchatPacket::new(
            MessageType::NoiseEncrypted,
            [0x33; SENDER_ID_LEN],
            None,
            1,
            b"pad me".to_vec(),
            EMBEDDED_TTL,
        );
        let bytes = codec::encode(&packet, true).expect("encodes");
        let padded = base64::engine::general_purpose::URL_SAFE.encode(&bytes);
        let content = format!("bitchat1:{padded}");
        assert_eq!(decode_embedded(&content).expect("decodes"), packet);
    }

    #[test]
    fn is_embedded_is_prefix_only() {
        assert!(is_embedded("bitchat1:"));
        assert!(!is_embedded("bitchat2:abc"));
        assert!(!is_embedded("verify:abc"));
        assert!(!is_embedded(""));
        // A `bitchat1:` prefix mid-string is not an embedded packet.
        assert!(!is_embedded("hello bitchat1:abc"));
    }

    #[test]
    fn decode_rejects_a_missing_prefix() {
        assert_eq!(
            decode_embedded("hello"),
            Err(NostrError::InvalidEvent(
                "content is not a bitchat1: payload"
            ))
        );
    }

    #[test]
    fn decode_rejects_malformed_base64() {
        for bad in [
            "bitchat1:!!!!",
            "bitchat1:a",
            "bitchat1:++++",
            "bitchat1:ab©d",
        ] {
            assert_eq!(
                decode_embedded(bad),
                Err(NostrError::InvalidEvent(
                    "embedded packet is not valid base64url"
                )),
                "{bad}"
            );
        }
    }

    #[test]
    fn decode_rejects_an_oversized_payload_before_decoding_it() {
        // Longer than the largest frame we would accept, so it must be refused on
        // length alone — never allocated.
        let huge = "A".repeat(MAX_FRAMED_FILE_BYTES.div_ceil(3) * 4 + 1);
        assert_eq!(
            decode_embedded(&format!("bitchat1:{huge}")),
            Err(NostrError::InvalidEvent(
                "embedded packet exceeds the maximum frame size"
            ))
        );
    }

    #[test]
    fn decode_rejects_valid_base64_that_is_not_a_packet() {
        // Well-formed base64, garbage packet: a decode error, not a panic.
        assert!(decode_embedded("bitchat1:AAAA").is_err());
        assert!(decode_embedded("bitchat1:").is_err());
    }

    #[test]
    fn private_message_packet_has_the_expected_shape() {
        let embedded =
            embed_private_message("hello there", "MSG-1", &recipient(), &sender(), 1_234_567)
                .expect("embeds");
        let packet = decode_embedded(&embedded).expect("decodes");

        assert_eq!(packet.version, 1);
        assert_eq!(packet.msg_type, MessageType::NoiseEncrypted.as_u8());
        assert_eq!(packet.ttl, EMBEDDED_TTL);
        assert_eq!(packet.timestamp, 1_234_567);
        assert_eq!(packet.signature, None);
        assert_eq!(packet.route, None);
        assert_eq!(
            packet.sender_id,
            [0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54, 0x32, 0x10]
        );
        assert_eq!(
            packet.recipient_id,
            Some([0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77])
        );

        let (payload_type, body) =
            decode_noise_payload(&packet.payload).expect("payload has a type byte");
        assert_eq!(payload_type, NoisePayloadType::PrivateMessage);
        let pm = PrivateMessagePacket::decode(body).expect("body is a private-message TLV");
        assert_eq!(pm.message_id, "MSG-1");
        assert_eq!(pm.content, "hello there");
    }

    #[test]
    fn private_message_survives_a_payload_long_enough_to_compress() {
        // Past the 100-byte compression threshold the codec deflates the payload.
        // The round trip has to survive that, since a real peer will decode it.
        // 255 bytes is the ceiling (see below), so sit just under it.
        let content = "yes ".repeat(60);
        assert!(content.len() > 100 && content.len() <= 255);
        let embedded = embed_private_message(&content, "MSG-LONG", &recipient(), &sender(), 9)
            .expect("embeds");
        let packet = decode_embedded(&embedded).expect("decodes");
        let (_, body) = decode_noise_payload(&packet.payload).expect("payload");
        assert_eq!(
            PrivateMessagePacket::decode(body).expect("TLV").content,
            content
        );
    }

    #[test]
    fn private_message_content_is_capped_at_255_bytes() {
        // Not our limit — bitchat's. `PrivateMessagePacket` writes TLV lengths as
        // a single byte (Packets.swift), so `encode()` returns nil above 255 and
        // the message is simply never sent. The app's own maxMessageLength is
        // 60_000, so this ceiling is invisible until a long DM silently fails.
        //
        // We reject rather than truncate: a truncated DM that looks delivered is
        // worse than one that visibly refuses to send.
        let at_limit = "a".repeat(255);
        assert!(embed_private_message(&at_limit, "MSG-255", &recipient(), &sender(), 1).is_ok());

        let over_limit = "a".repeat(256);
        assert!(embed_private_message(&over_limit, "MSG-256", &recipient(), &sender(), 1).is_err());
    }

    #[test]
    fn private_message_without_a_recipient_omits_the_field_entirely() {
        let embedded = embed_private_message_no_recipient("geohash dm", "MSG-2", &sender(), 5)
            .expect("embeds");
        let packet = decode_embedded(&embedded).expect("decodes");
        assert_eq!(packet.recipient_id, None);
        assert!(!packet.is_broadcast());
    }

    #[test]
    fn acks_carry_the_message_id_as_raw_utf8() {
        for kind in [NoisePayloadType::Delivered, NoisePayloadType::ReadReceipt] {
            let embedded =
                embed_ack(kind, "TEST-MSG-1", Some(&recipient()), &sender(), 11).expect("embeds");
            let packet = decode_embedded(&embedded).expect("decodes");

            assert_eq!(packet.msg_type, MessageType::NoiseEncrypted.as_u8());
            assert_eq!(packet.ttl, EMBEDDED_TTL);
            let (payload_type, body) = decode_noise_payload(&packet.payload).expect("payload");
            assert_eq!(payload_type, kind);
            assert_eq!(body, b"TEST-MSG-1");
        }
    }

    #[test]
    fn acks_without_a_recipient_are_the_geohash_variant() {
        let embedded = embed_ack(
            NoisePayloadType::ReadReceipt,
            "TEST-MSG-READ-1",
            None,
            &sender(),
            3,
        )
        .expect("embeds");
        let packet = decode_embedded(&embedded).expect("decodes");
        assert_eq!(packet.recipient_id, None);
        let (payload_type, body) = decode_noise_payload(&packet.payload).expect("payload");
        assert_eq!(payload_type, NoisePayloadType::ReadReceipt);
        assert_eq!(body, b"TEST-MSG-READ-1");
    }

    #[test]
    fn acks_reject_every_other_payload_type() {
        for kind in [
            NoisePayloadType::PrivateMessage,
            NoisePayloadType::VoiceFrame,
            NoisePayloadType::VerifyChallenge,
            NoisePayloadType::Vouch,
        ] {
            assert_eq!(
                embed_ack(kind, "M", None, &sender(), 0),
                Err(NostrError::InvalidEvent(
                    "ack payload type must be delivered or read receipt"
                )),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_thirty_two_byte_recipient_is_hashed_to_its_peer_id() {
        let key = [0xAB; 32];
        let key_hex = key.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let as_key = PeerId::from_str(&key_hex);

        // `PeerID(publicKey:)` — SHA-256 of the key, first eight bytes.
        assert_eq!(
            normalize_recipient_peer_id(&as_key),
            PeerId::from_noise_key(&key)
        );
        assert_eq!(
            normalize_recipient_peer_id(&as_key).as_str(),
            "9a2db2e23f1504cd"
        );

        let embedded = embed_ack(
            NoisePayloadType::Delivered,
            "M",
            Some(&as_key),
            &sender(),
            0,
        )
        .expect("embeds");
        let packet = decode_embedded(&embedded).expect("decodes");
        assert_eq!(
            packet.recipient_id,
            Some([0x9A, 0x2D, 0xB2, 0xE2, 0x3F, 0x15, 0x04, 0xCD]),
            "the derived peer ID, not the truncated key"
        );
        assert_ne!(packet.recipient_id, Some([0xAB; SENDER_ID_LEN]));
    }

    #[test]
    fn an_eight_byte_recipient_passes_through_unchanged() {
        let peer = recipient();
        assert_eq!(normalize_recipient_peer_id(&peer), peer);
    }

    #[test]
    fn a_non_hex_recipient_yields_no_recipient_field() {
        // Geohash peer IDs carry a `nostr_` prefix, so the whole string is not hex
        // and Swift's `Data(hexString:)` returns nil.
        for id in ["nostr_abcdef0123456789", "name:alice", "not-hex"] {
            let peer = PeerId::from_str(id);
            // Normalisation is a no-op...
            assert_eq!(normalize_recipient_peer_id(&peer), peer, "{id}");
            // ...and the packet ends up with no recipient at all.
            let embedded = embed_ack(NoisePayloadType::Delivered, "M", Some(&peer), &sender(), 0)
                .expect("embeds");
            assert_eq!(
                decode_embedded(&embedded).expect("decodes").recipient_id,
                None,
                "{id}"
            );
        }
    }

    #[test]
    fn a_non_hex_sender_becomes_eight_zero_bytes() {
        let peer = PeerId::from_str("nostr_abcdef0123456789");
        let embedded = embed_ack(NoisePayloadType::Delivered, "M", None, &peer, 0).expect("embeds");
        assert_eq!(
            decode_embedded(&embedded).expect("decodes").sender_id,
            [0u8; SENDER_ID_LEN]
        );
    }

    #[test]
    fn a_short_hex_sender_is_zero_padded_and_a_long_one_truncated() {
        let short = PeerId::from_str("aabb");
        let embedded =
            embed_ack(NoisePayloadType::Delivered, "M", None, &short, 0).expect("embeds");
        assert_eq!(
            decode_embedded(&embedded).expect("decodes").sender_id,
            [0xAA, 0xBB, 0, 0, 0, 0, 0, 0]
        );

        let long = PeerId::from_str(&"11".repeat(12));
        let embedded = embed_ack(NoisePayloadType::Delivered, "M", None, &long, 0).expect("embeds");
        assert_eq!(
            decode_embedded(&embedded).expect("decodes").sender_id,
            [0x11; SENDER_ID_LEN]
        );
    }

    #[test]
    fn hex_decoding_matches_swift_semantics() {
        assert_eq!(hex_to_bytes("00ff"), Some(vec![0x00, 0xFF]));
        assert_eq!(hex_to_bytes("0x00FF"), Some(vec![0x00, 0xFF]));
        assert_eq!(hex_to_bytes(" 00ff\n"), Some(vec![0x00, 0xFF]));
        assert_eq!(hex_to_bytes(""), Some(Vec::new()));
        assert_eq!(hex_to_bytes("abc"), None);
        assert_eq!(hex_to_bytes("zz"), None);
        assert_eq!(hex_to_bytes("çç"), None);
    }

    #[test]
    fn message_ids_and_content_survive_unicode() {
        let embedded =
            embed_private_message("héllo 🌍", "id-é", &recipient(), &sender(), 1).expect("embeds");
        let packet = decode_embedded(&embedded).expect("decodes");
        let (_, body) = decode_noise_payload(&packet.payload).expect("payload");
        let pm = PrivateMessagePacket::decode(body).expect("TLV");
        assert_eq!(pm.content, "héllo 🌍");
        assert_eq!(pm.message_id, "id-é");
    }

    #[test]
    fn an_over_long_message_id_is_an_error_not_a_panic() {
        // The private-message TLV uses one-byte lengths, so anything past 255 bytes
        // cannot be encoded. It has to surface as an error.
        let long_id = "x".repeat(300);
        assert!(embed_private_message("hi", &long_id, &recipient(), &sender(), 0).is_err());
    }
}
