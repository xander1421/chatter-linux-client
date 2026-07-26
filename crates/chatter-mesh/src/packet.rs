//! The bitchat packet and its type tags.
//!
//! Ported from `BitFoundation/BitchatPacket.swift`, `MessageType.swift`, and the
//! `NoisePayloadType` enum in `Protocols/BitchatProtocol.swift`.
//!
//! These types are the contract every other module in this crate depends on. They
//! are deliberately plain data: construction never validates beyond field widths,
//! because the codec owns wire validation and the policy modules own routing rules.

use sha2::{Digest, Sha256};

use crate::error::MeshError;

/// Sender and recipient IDs are 8 bytes on the wire, zero-padded when shorter.
pub const SENDER_ID_LEN: usize = 8;
/// Ed25519 signature width.
pub const SIGNATURE_LEN: usize = 64;
/// A recipient of eight `0xFF` bytes means "everyone".
///
/// Not the same *encoding* as `recipient_id: None`, which means the packet carries no
/// recipient field at all (the `HAS_RECIPIENT` flag is clear). Both occur on the wire
/// and both mean "everyone" — see [`BitchatPacket::is_broadcast`] — but they must
/// never be rewritten into each other: the recipient bytes are inside the signature
/// preimage, so substituting one form for the other invalidates the originator's
/// signature. Source routing also reaches them differently: an explicit broadcast
/// recipient is checked and found ineligible for a route, while an absent recipient
/// never reaches that check at all.
///
/// bitchat originates the absent form for announces and public messages.
pub const BROADCAST_RECIPIENT: [u8; SENDER_ID_LEN] = [0xFF; SENDER_ID_LEN];

/// A decoded bitchat packet.
///
/// `msg_type` stays a raw `u8` rather than a `MessageType`. Relays must forward
/// packet types they do not understand — that is how bitchat ships new types to a
/// mixed-version mesh without partitioning it — and an enum-typed field would make
/// an unknown type impossible to represent, let alone re-encode byte-for-byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitchatPacket {
    /// 1 or 2. Version 2 adds a 4-byte payload length and source routing.
    pub version: u8,
    /// Raw type tag; see [`MessageType`] for the known values.
    pub msg_type: u8,
    /// Hop limit. Decremented by relays, and excluded from the signature preimage
    /// precisely so that decrementing it does not invalidate the signature.
    pub ttl: u8,
    /// Milliseconds since the Unix epoch.
    pub timestamp: u64,
    pub sender_id: [u8; SENDER_ID_LEN],
    /// `None` when the `HAS_RECIPIENT` flag is clear. See [`BROADCAST_RECIPIENT`].
    pub recipient_id: Option<[u8; SENDER_ID_LEN]>,
    pub payload: Vec<u8>,
    pub signature: Option<[u8; SIGNATURE_LEN]>,
    /// Intermediate hops only — never the sender, never the recipient. v2 only.
    pub route: Option<Vec<[u8; SENDER_ID_LEN]>>,
    /// Request-Sync Response: exempts the packet from the 2-minute freshness check,
    /// but only when it answers a sync request we actually sent to this peer.
    pub is_rsr: bool,
}

impl BitchatPacket {
    /// Builds a v1 packet with no route and no signature.
    ///
    /// v1 is the default because routes are the only reason to emit v2, and routes
    /// are attached later by the origination policy — which re-signs, since the
    /// route is inside the signed bytes.
    pub fn new(
        msg_type: MessageType,
        sender_id: [u8; SENDER_ID_LEN],
        recipient_id: Option<[u8; SENDER_ID_LEN]>,
        timestamp_ms: u64,
        payload: Vec<u8>,
        ttl: u8,
    ) -> Self {
        Self {
            version: 1,
            msg_type: msg_type.as_u8(),
            ttl,
            timestamp: timestamp_ms,
            sender_id,
            recipient_id,
            payload,
            signature: None,
            route: None,
            is_rsr: false,
        }
    }

    /// The known type for this packet, or `None` if a peer sent something newer
    /// than we understand. Unknown types still relay.
    pub fn message_type(&self) -> Option<MessageType> {
        MessageType::from_u8(self.msg_type)
    }

    /// True when the packet is addressed to everyone.
    ///
    /// Two encodings mean that, and both are on the wire. bitchat *sends* an
    /// absent recipient — `recipientID: nil`, `HAS_RECIPIENT` clear — for
    /// announces and public messages (`BLEService.swift:502` and `:1677`), and
    /// *accepts* either that or the explicit all-`0xFF` recipient
    /// (`BLEPacketFreshnessPolicy.isBroadcastRecipient`). Treating an absent
    /// recipient as "not broadcast" silently drops every real public message.
    pub fn is_broadcast(&self) -> bool {
        self.recipient_id
            .is_none_or(|recipient| recipient == BROADCAST_RECIPIENT)
    }

    /// The exact bytes an Ed25519 signature covers.
    ///
    /// Mirrors `BitchatPacket.toBinaryDataForSigning()`: re-encode with `ttl = 0`,
    /// no signature, and `is_rsr = false`, because all three mutate in flight.
    ///
    /// The subtle part, and the one most likely to be got wrong: that Swift method
    /// calls `BinaryProtocol.encode(unsignedPacket)` **without** passing `padding`,
    /// so it takes the default of `true`. The preimage is therefore the *padded*
    /// encoding. Signing the unpadded bytes produces signatures that verify against
    /// our own implementation and fail against every real peer.
    pub fn signing_preimage(&self) -> Result<Vec<u8>, MeshError> {
        let unsigned = Self {
            ttl: 0,
            signature: None,
            is_rsr: false,
            ..self.clone()
        };
        crate::codec::encode(&unsigned, true)
    }

    /// Deterministic 16-byte identity used for gossip-sync set membership.
    ///
    /// `SHA256(type ‖ sender_id ‖ timestamp_be ‖ payload)[..16]`, per
    /// `Sync/PacketIdUtil.swift`. Note what is excluded: ttl, recipient, route, and
    /// signature. Two relays of the same original packet therefore share an ID even
    /// though their ttl bytes differ, which is the whole point.
    pub fn packet_id(&self) -> [u8; 16] {
        let mut hasher = Sha256::new();
        hasher.update([self.msg_type]);
        hasher.update(self.sender_id);
        hasher.update(self.timestamp.to_be_bytes());
        hasher.update(&self.payload);
        let digest = hasher.finalize();

        let mut id = [0u8; 16];
        id.copy_from_slice(&digest[..16]);
        id
    }
}

/// Wire type tags. Consolidated by bitchat from 24 original types down to these.
///
/// All private-message metadata (receipts, delivery status) rides inside
/// `NoiseEncrypted` payloads rather than getting its own wire type — see
/// [`NoisePayloadType`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MessageType {
    /// "I'm here", with nickname and keys. Signed.
    Announce = 0x01,
    /// Public chat message. Signed.
    Message = 0x02,
    /// "I'm leaving".
    Leave = 0x03,
    /// Store-and-forward envelope carried on behalf of an offline third party.
    CourierEnvelope = 0x04,
    /// Noise handshake; initiator vs responder is determined by the payload.
    NoiseHandshake = 0x10,
    /// Any encrypted payload — messages, receipts, voice frames, group state.
    NoiseEncrypted = 0x11,
    /// One piece of a packet too large for the link MTU.
    Fragment = 0x20,
    /// GCS-filter sync request. Link-local: TTL 0, never relayed.
    RequestSync = 0x21,
    /// Binary file, image, or audio payload.
    FileTransfer = 0x22,
    /// Signed geohash bulletin-board post or tombstone.
    BoardPost = 0x23,
    /// Signed batch of one-time prekeys, gossiped for forward-secret courier seals.
    PrekeyBundle = 0x24,
    /// Group-encrypted broadcast: cleartext group ID, ChaCha20-Poly1305 body.
    GroupMessage = 0x25,
    /// Directed echo request (nonce + origin TTL).
    Ping = 0x26,
    /// Directed echo reply.
    Pong = 0x27,
    /// Signed Nostr event ferried between a mesh-only peer and an internet gateway.
    NostrCarrier = 0x28,
    /// One signed push-to-talk burst. Ephemeral broadcast, never gossip-synced.
    VoiceFrame = 0x29,
    /// Metadata for a chunked file transfer: size, chunking, whole-file digest.
    /// Signed. See [`crate::tlv::file_stream`].
    FileManifest = 0x2A,
    /// One chunk of a chunked file transfer. Signed, individually.
    FileChunk = 0x2B,
    /// Receiver-driven request for chunks that never arrived. Signed.
    FileChunkRequest = 0x2C,
}

impl MessageType {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x01 => Self::Announce,
            0x02 => Self::Message,
            0x03 => Self::Leave,
            0x04 => Self::CourierEnvelope,
            0x10 => Self::NoiseHandshake,
            0x11 => Self::NoiseEncrypted,
            0x20 => Self::Fragment,
            0x21 => Self::RequestSync,
            0x22 => Self::FileTransfer,
            0x23 => Self::BoardPost,
            0x24 => Self::PrekeyBundle,
            0x25 => Self::GroupMessage,
            0x26 => Self::Ping,
            0x27 => Self::Pong,
            0x28 => Self::NostrCarrier,
            0x29 => Self::VoiceFrame,
            // 0x2A and beyond are unused by bitchat today, and claiming them is
            // a risk worth stating rather than hiding: **if upstream bitchat
            // later defines 0x2A, two different protocols share a tag.** The
            // mitigation is that an unrecognised type is relayed rather than
            // rejected, and each of these payloads is signature-verified and
            // structurally validated before it does anything, so a collision
            // degrades to "peers ignore each other's version" instead of
            // mis-parsing. Revisit the tag choice before calling this stable.
            0x2A => Self::FileManifest,
            0x2B => Self::FileChunk,
            0x2C => Self::FileChunkRequest,
            _ => return None,
        })
    }

    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The type byte prefixed to a payload *inside* a Noise session.
///
/// Relays never see these: they are past the `NoiseEncrypted` boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NoisePayloadType {
    PrivateMessage = 0x01,
    ReadReceipt = 0x02,
    Delivered = 0x03,
    /// Creator-signed group state (invite).
    GroupInvite = 0x06,
    /// Creator-signed group state (key rotation / roster update).
    GroupKeyUpdate = 0x07,
    /// One live voice-burst packet.
    VoiceFrame = 0x08,
    VerifyChallenge = 0x10,
    VerifyResponse = 0x11,
    /// Batch of vouch attestations.
    Vouch = 0x12,
}

impl NoisePayloadType {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x01 => Self::PrivateMessage,
            0x02 => Self::ReadReceipt,
            0x03 => Self::Delivered,
            0x06 => Self::GroupInvite,
            0x07 => Self::GroupKeyUpdate,
            0x08 => Self::VoiceFrame,
            0x10 => Self::VerifyChallenge,
            0x11 => Self::VerifyResponse,
            0x12 => Self::Vouch,
            _ => return None,
        })
    }

    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Prefixes a payload with its Noise type byte.
pub fn encode_noise_payload(payload_type: NoisePayloadType, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(payload_type.as_u8());
    out.extend_from_slice(body);
    out
}

/// Splits a Noise payload into its type byte and body.
///
/// Returns `None` for an empty payload or an unknown type byte, matching
/// `NoisePayload.decode`.
pub fn decode_noise_payload(data: &[u8]) -> Option<(NoisePayloadType, &[u8])> {
    let (first, rest) = data.split_first()?;
    Some((NoisePayloadType::from_u8(*first)?, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_type_round_trips_every_variant() {
        let all = [
            MessageType::Announce,
            MessageType::Message,
            MessageType::Leave,
            MessageType::CourierEnvelope,
            MessageType::NoiseHandshake,
            MessageType::NoiseEncrypted,
            MessageType::Fragment,
            MessageType::RequestSync,
            MessageType::FileTransfer,
            MessageType::BoardPost,
            MessageType::PrekeyBundle,
            MessageType::GroupMessage,
            MessageType::Ping,
            MessageType::Pong,
            MessageType::NostrCarrier,
            MessageType::VoiceFrame,
            MessageType::FileManifest,
            MessageType::FileChunk,
            MessageType::FileChunkRequest,
        ];
        for t in all {
            assert_eq!(MessageType::from_u8(t.as_u8()), Some(t));
        }
        assert_eq!(MessageType::from_u8(0x00), None);
        assert_eq!(MessageType::from_u8(0xFE), None);
        // The next tag after ours. Named so that the day bitchat defines it, the
        // collision recorded on `from_u8` is one grep away.
        assert_eq!(MessageType::from_u8(0x2D), None);
    }

    #[test]
    fn noise_payload_type_round_trips_every_variant() {
        let all = [
            NoisePayloadType::PrivateMessage,
            NoisePayloadType::ReadReceipt,
            NoisePayloadType::Delivered,
            NoisePayloadType::GroupInvite,
            NoisePayloadType::GroupKeyUpdate,
            NoisePayloadType::VoiceFrame,
            NoisePayloadType::VerifyChallenge,
            NoisePayloadType::VerifyResponse,
            NoisePayloadType::Vouch,
        ];
        for t in all {
            assert_eq!(NoisePayloadType::from_u8(t.as_u8()), Some(t));
        }
        // 0x04 and 0x05 are deliberately absent from the enum.
        assert_eq!(NoisePayloadType::from_u8(0x04), None);
        assert_eq!(NoisePayloadType::from_u8(0x05), None);
    }

    #[test]
    fn both_wire_encodings_of_broadcast_are_broadcast() {
        let mut packet = BitchatPacket::new(
            MessageType::Message,
            [1; SENDER_ID_LEN],
            None,
            1_700_000_000_000,
            b"hi".to_vec(),
            7,
        );
        // The form bitchat actually sends: no recipient field at all. Reading
        // this as "not broadcast" drops every real public message and announce.
        assert!(packet.is_broadcast());

        packet.recipient_id = Some(BROADCAST_RECIPIENT);
        assert!(packet.is_broadcast());

        packet.recipient_id = Some([0x11; SENDER_ID_LEN]);
        assert!(!packet.is_broadcast());
    }

    #[test]
    fn packet_id_ignores_ttl_route_recipient_and_signature() {
        let base = BitchatPacket::new(
            MessageType::Message,
            [0xAB; SENDER_ID_LEN],
            None,
            42,
            b"payload".to_vec(),
            7,
        );
        let relayed = BitchatPacket {
            ttl: 3,
            signature: Some([0x5A; SIGNATURE_LEN]),
            recipient_id: Some([0x22; SENDER_ID_LEN]),
            route: Some(vec![[0x33; SENDER_ID_LEN]]),
            is_rsr: true,
            ..base.clone()
        };
        assert_eq!(base.packet_id(), relayed.packet_id());
    }

    #[test]
    fn packet_id_changes_with_payload_sender_type_and_timestamp() {
        let base = BitchatPacket::new(
            MessageType::Message,
            [0xAB; SENDER_ID_LEN],
            None,
            42,
            b"payload".to_vec(),
            7,
        );

        let other_payload = BitchatPacket {
            payload: b"payloae".to_vec(),
            ..base.clone()
        };
        let other_sender = BitchatPacket {
            sender_id: [0xAC; SENDER_ID_LEN],
            ..base.clone()
        };
        let other_type = BitchatPacket {
            msg_type: MessageType::Announce.as_u8(),
            ..base.clone()
        };
        let other_time = BitchatPacket {
            timestamp: 43,
            ..base.clone()
        };

        for variant in [other_payload, other_sender, other_type, other_time] {
            assert_ne!(base.packet_id(), variant.packet_id());
        }
    }

    #[test]
    fn noise_payload_round_trips() {
        let encoded = encode_noise_payload(NoisePayloadType::Delivered, b"msg-id");
        assert_eq!(encoded[0], 0x03);

        let (payload_type, body) =
            decode_noise_payload(&encoded).expect("well-formed payload decodes");
        assert_eq!(payload_type, NoisePayloadType::Delivered);
        assert_eq!(body, b"msg-id");
    }

    #[test]
    fn noise_payload_rejects_empty_and_unknown() {
        assert!(decode_noise_payload(&[]).is_none());
        assert!(decode_noise_payload(&[0x7F, 0x01]).is_none());
    }

    #[test]
    fn noise_payload_accepts_type_byte_with_empty_body() {
        let (payload_type, body) =
            decode_noise_payload(&[0x01]).expect("type byte alone is a valid payload");
        assert_eq!(payload_type, NoisePayloadType::PrivateMessage);
        assert!(body.is_empty());
    }
}
