//! One packet of a live push-to-talk voice burst.
//!
//! Ported from `Protocols/VoiceBurstPacket.swift`. This is the inner payload of
//! a `NoisePayloadType::VoiceFrame` (0x08) for private bursts, and the payload
//! of a `MessageType::VoiceFrame` (0x29) for public mesh bursts.
//!
//! Not TLV — a fixed 11-byte header and a flag-dependent body:
//!
//! ```text
//! [burstID: 8][seq: u16 BE][flags: u8][payload…]
//! ```
//!
//! | Flags | Payload |
//! |---|---|
//! | 0x01 START | `[codec: u8]` |
//! | 0x02 END | `[totalDataPackets: u16 BE][durationMs: u32 BE]` |
//! | 0x04 CANCELED | empty; receivers discard the burst |
//! | 0x00 data | repeated `[length: u16 BE][AAC frame]` |
//!
//! The flags byte is matched as a whole value, not tested bit by bit: 0x03 is
//! not "START and END", it is unrecognised and rejected.
//!
//! `seq` 0 is reserved for the START packet; data packets begin at 1.

use crate::error::MeshError;
use crate::tlv::Reader;

/// Width of the burst identifier.
pub const BURST_ID_LEN: usize = 8;
/// Fixed header: burst ID, sequence number, flags.
pub const HEADER_LEN: usize = BURST_ID_LEN + 2 + 1;
/// Sanity cap on frames per packet; real packets carry one or two.
pub const MAX_FRAMES_PER_PACKET: usize = 8;

const FLAG_START: u8 = 0x01;
const FLAG_END: u8 = 0x02;
const FLAG_CANCELED: u8 = 0x04;
const FLAG_DATA: u8 = 0x00;

/// Audio codec of a live burst. START packets carry it so receivers can reject
/// bursts they cannot decode instead of feeding garbage to the decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum VoiceBurstCodec {
    /// AAC-LC, 16 kHz, mono, ~16 kbps — the same encoder settings as the voice
    /// note recorder, so the finalised `.m4a` and the live frames match.
    AacLc16kMono = 0x01,
}

impl VoiceBurstCodec {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::AacLc16kMono),
            _ => None,
        }
    }

    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceBurstKind {
    Start {
        codec: VoiceBurstCodec,
    },
    /// One to eight non-empty AAC frames.
    Frames(Vec<Vec<u8>>),
    End {
        total_data_packets: u16,
        duration_ms: u32,
    },
    Canceled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceBurstPacket {
    pub burst_id: [u8; BURST_ID_LEN],
    pub seq: u16,
    pub kind: VoiceBurstKind,
}

impl VoiceBurstPacket {
    /// Mirrors Swift's failable initialiser: the frame-list invariants are
    /// checked at construction, so an invalid packet cannot exist.
    pub fn new(
        burst_id: [u8; BURST_ID_LEN],
        seq: u16,
        kind: VoiceBurstKind,
    ) -> Result<Self, MeshError> {
        if let VoiceBurstKind::Frames(frames) = &kind {
            if frames.is_empty() {
                return Err(MeshError::OutOfRange("voice burst data packet has no frames"));
            }
            if frames.len() > MAX_FRAMES_PER_PACKET {
                return Err(MeshError::OutOfRange(
                    "voice burst carries more than 8 frames",
                ));
            }
            for frame in frames {
                if frame.is_empty() {
                    return Err(MeshError::OutOfRange("voice burst frame is empty"));
                }
                if frame.len() > u16::MAX as usize {
                    return Err(MeshError::LengthExceeded {
                        declared: frame.len(),
                        max: u16::MAX as usize,
                    });
                }
            }
        }
        Ok(Self {
            burst_id,
            seq,
            kind,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        // Re-validate: the fields are public, so a caller can build a packet
        // without going through `new`.
        let validated = Self::new(self.burst_id, self.seq, self.kind.clone())?;

        let mut out = Vec::with_capacity(HEADER_LEN + validated.payload_size());
        out.extend_from_slice(&validated.burst_id);
        out.extend_from_slice(&validated.seq.to_be_bytes());

        match &validated.kind {
            VoiceBurstKind::Start { codec } => {
                out.push(FLAG_START);
                out.push(codec.as_u8());
            }
            VoiceBurstKind::Frames(frames) => {
                out.push(FLAG_DATA);
                for frame in frames {
                    out.extend_from_slice(&(frame.len() as u16).to_be_bytes());
                    out.extend_from_slice(frame);
                }
            }
            VoiceBurstKind::End {
                total_data_packets,
                duration_ms,
            } => {
                out.push(FLAG_END);
                out.extend_from_slice(&total_data_packets.to_be_bytes());
                out.extend_from_slice(&duration_ms.to_be_bytes());
            }
            VoiceBurstKind::Canceled => out.push(FLAG_CANCELED),
        }

        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);
        let burst_id = reader.array::<BURST_ID_LEN>()?;
        let seq = reader.u16_be()?;
        let flags = reader.u8()?;

        let kind = match flags {
            FLAG_START => {
                // Trailing bytes after the codec are ignored, as in Swift.
                let codec_byte = reader.u8()?;
                let codec = VoiceBurstCodec::from_u8(codec_byte)
                    .ok_or(MeshError::OutOfRange("unknown voice burst codec"))?;
                VoiceBurstKind::Start { codec }
            }
            FLAG_END => {
                let total_data_packets = reader.u16_be()?;
                let duration_ms = reader.u32_be()?;
                VoiceBurstKind::End {
                    total_data_packets,
                    duration_ms,
                }
            }
            FLAG_CANCELED => VoiceBurstKind::Canceled,
            FLAG_DATA => {
                let mut frames: Vec<Vec<u8>> = Vec::new();
                while !reader.is_empty() {
                    let length = reader.u16_be()? as usize;
                    // Check order matches Swift's `guard length > 0,
                    // remaining >= length, frames.count < max`, so a malformed
                    // packet reports the same reason theirs would.
                    if length == 0 {
                        return Err(MeshError::OutOfRange("voice burst frame is empty"));
                    }
                    if reader.remaining() < length {
                        return Err(MeshError::Truncated {
                            offset: reader.position(),
                            needed: length - reader.remaining(),
                        });
                    }
                    if frames.len() >= MAX_FRAMES_PER_PACKET {
                        return Err(MeshError::OutOfRange(
                            "voice burst carries more than 8 frames",
                        ));
                    }
                    frames.push(reader.take(length)?.to_vec());
                }
                if frames.is_empty() {
                    return Err(MeshError::OutOfRange("voice burst data packet has no frames"));
                }
                VoiceBurstKind::Frames(frames)
            }
            _ => return Err(MeshError::OutOfRange("unknown voice burst flags")),
        };

        Self::new(burst_id, seq, kind)
    }

    fn payload_size(&self) -> usize {
        match &self.kind {
            VoiceBurstKind::Start { .. } => 1,
            VoiceBurstKind::Frames(frames) => {
                frames.iter().map(|frame| 2 + frame.len()).sum()
            }
            VoiceBurstKind::End { .. } => 6,
            VoiceBurstKind::Canceled => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: [u8; BURST_ID_LEN] = [0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8];

    #[test]
    fn start_packet_round_trips() {
        let packet = VoiceBurstPacket::new(
            ID,
            0,
            VoiceBurstKind::Start {
                codec: VoiceBurstCodec::AacLc16kMono,
            },
        )
        .unwrap();
        let encoded = packet.encode().unwrap();
        assert_eq!(
            encoded,
            vec![0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0x00, 0x00, 0x01, 0x01]
        );
        assert_eq!(VoiceBurstPacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn data_packet_round_trips() {
        let packet = VoiceBurstPacket::new(
            ID,
            1,
            VoiceBurstKind::Frames(vec![vec![0x11, 0x22], vec![0x33]]),
        )
        .unwrap();
        let encoded = packet.encode().unwrap();
        assert_eq!(
            encoded.get(HEADER_LEN..),
            Some(&[0x00, 0x02, 0x11, 0x22, 0x00, 0x01, 0x33][..])
        );
        assert_eq!(encoded.get(10).copied(), Some(FLAG_DATA));
        assert_eq!(VoiceBurstPacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn end_packet_round_trips() {
        let packet = VoiceBurstPacket::new(
            ID,
            9,
            VoiceBurstKind::End {
                total_data_packets: 0x0102,
                duration_ms: 0x0304_0506,
            },
        )
        .unwrap();
        let encoded = packet.encode().unwrap();
        assert_eq!(
            encoded.get(HEADER_LEN..),
            Some(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06][..])
        );
        assert_eq!(VoiceBurstPacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn canceled_packet_round_trips() {
        let packet = VoiceBurstPacket::new(ID, 4, VoiceBurstKind::Canceled).unwrap();
        let encoded = packet.encode().unwrap();
        assert_eq!(encoded.len(), HEADER_LEN);
        assert_eq!(VoiceBurstPacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn sequence_number_is_big_endian() {
        let packet = VoiceBurstPacket::new(ID, 0x1234, VoiceBurstKind::Canceled).unwrap();
        let encoded = packet.encode().unwrap();
        assert_eq!(encoded.get(8..10), Some(&[0x12, 0x34][..]));
    }

    #[test]
    fn header_shorter_than_eleven_bytes_is_rejected() {
        for len in 0..HEADER_LEN {
            assert!(matches!(
                VoiceBurstPacket::decode(&vec![0u8; len]),
                Err(MeshError::Truncated { .. })
            ));
        }
    }

    #[test]
    fn start_without_a_codec_byte_is_truncation() {
        let mut bytes = vec![0u8; HEADER_LEN];
        if let Some(slot) = bytes.get_mut(10) {
            *slot = FLAG_START;
        }
        assert!(matches!(
            VoiceBurstPacket::decode(&bytes),
            Err(MeshError::Truncated { .. })
        ));
    }

    #[test]
    fn unknown_codec_is_rejected() {
        let mut bytes = vec![0u8; HEADER_LEN];
        if let Some(slot) = bytes.get_mut(10) {
            *slot = FLAG_START;
        }
        bytes.push(0x99);
        assert!(matches!(
            VoiceBurstPacket::decode(&bytes),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn trailing_bytes_after_a_start_codec_are_ignored() {
        let mut bytes = vec![0u8; HEADER_LEN];
        if let Some(slot) = bytes.get_mut(10) {
            *slot = FLAG_START;
        }
        bytes.extend_from_slice(&[0x01, 0xDE, 0xAD]);
        let decoded = VoiceBurstPacket::decode(&bytes).unwrap();
        assert_eq!(
            decoded.kind,
            VoiceBurstKind::Start {
                codec: VoiceBurstCodec::AacLc16kMono
            }
        );
    }

    #[test]
    fn end_packet_shorter_than_six_payload_bytes_is_rejected() {
        let packet = VoiceBurstPacket::new(
            ID,
            1,
            VoiceBurstKind::End {
                total_data_packets: 1,
                duration_ms: 1,
            },
        )
        .unwrap();
        let encoded = packet.encode().unwrap();
        for cut in HEADER_LEN..encoded.len() {
            assert!(matches!(
                VoiceBurstPacket::decode(&encoded[..cut]),
                Err(MeshError::Truncated { .. })
            ));
        }
    }

    #[test]
    fn end_packet_ignores_trailing_bytes() {
        let packet = VoiceBurstPacket::new(
            ID,
            1,
            VoiceBurstKind::End {
                total_data_packets: 7,
                duration_ms: 8,
            },
        )
        .unwrap();
        let mut encoded = packet.encode().unwrap();
        encoded.push(0xFF);
        assert_eq!(VoiceBurstPacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn canceled_packet_ignores_its_payload() {
        let mut bytes = vec![0u8; HEADER_LEN];
        if let Some(slot) = bytes.get_mut(10) {
            *slot = FLAG_CANCELED;
        }
        bytes.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            VoiceBurstPacket::decode(&bytes).unwrap().kind,
            VoiceBurstKind::Canceled
        );
    }

    #[test]
    fn unknown_flags_are_rejected_rather_than_masked() {
        for flags in [0x03u8, 0x05, 0x06, 0x07, 0x08, 0x80, 0xFF] {
            let mut bytes = vec![0u8; HEADER_LEN];
            if let Some(slot) = bytes.get_mut(10) {
                *slot = flags;
            }
            assert!(
                matches!(
                    VoiceBurstPacket::decode(&bytes),
                    Err(MeshError::OutOfRange(_))
                ),
                "flags {flags:#04x} must not decode"
            );
        }
    }

    #[test]
    fn a_data_packet_with_no_frames_is_rejected() {
        let bytes = vec![0u8; HEADER_LEN];
        assert!(matches!(
            VoiceBurstPacket::decode(&bytes),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn a_zero_length_frame_is_rejected() {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes.extend_from_slice(&[0x00, 0x00]);
        assert!(matches!(
            VoiceBurstPacket::decode(&bytes),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn a_frame_length_past_the_end_is_truncation() {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes.extend_from_slice(&[0x00, 0x10, 0xAA]);
        assert!(matches!(
            VoiceBurstPacket::decode(&bytes),
            Err(MeshError::Truncated { .. })
        ));
    }

    #[test]
    fn a_dangling_frame_length_byte_is_truncation() {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes.extend_from_slice(&[0x00, 0x01, 0xAA, 0x00]);
        assert!(matches!(
            VoiceBurstPacket::decode(&bytes),
            Err(MeshError::Truncated { .. })
        ));
    }

    #[test]
    fn eight_frames_are_allowed_and_nine_are_not() {
        let frames: Vec<Vec<u8>> = (0..MAX_FRAMES_PER_PACKET).map(|i| vec![i as u8]).collect();
        let packet =
            VoiceBurstPacket::new(ID, 1, VoiceBurstKind::Frames(frames.clone())).unwrap();
        let mut encoded = packet.encode().unwrap();
        assert_eq!(VoiceBurstPacket::decode(&encoded).unwrap(), packet);

        // A ninth frame on the wire is rejected.
        encoded.extend_from_slice(&[0x00, 0x01, 0xFF]);
        assert!(matches!(
            VoiceBurstPacket::decode(&encoded),
            Err(MeshError::OutOfRange(_))
        ));

        // And rejected at construction.
        let mut too_many = frames;
        too_many.push(vec![0xFF]);
        assert!(matches!(
            VoiceBurstPacket::new(ID, 1, VoiceBurstKind::Frames(too_many)),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn construction_rejects_empty_frame_lists_and_empty_frames() {
        assert!(VoiceBurstPacket::new(ID, 1, VoiceBurstKind::Frames(vec![])).is_err());
        assert!(
            VoiceBurstPacket::new(ID, 1, VoiceBurstKind::Frames(vec![vec![1], vec![]])).is_err()
        );
    }

    #[test]
    fn encode_revalidates_a_hand_built_packet() {
        let mut packet = VoiceBurstPacket::new(ID, 1, VoiceBurstKind::Canceled).unwrap();
        packet.kind = VoiceBurstKind::Frames(vec![Vec::new()]);
        assert!(matches!(packet.encode(), Err(MeshError::OutOfRange(_))));
    }

    #[test]
    fn a_maximum_size_frame_round_trips() {
        let packet = VoiceBurstPacket::new(
            ID,
            u16::MAX,
            VoiceBurstKind::Frames(vec![vec![0x5A; u16::MAX as usize]]),
        )
        .unwrap();
        let encoded = packet.encode().unwrap();
        assert_eq!(encoded.len(), HEADER_LEN + 2 + u16::MAX as usize);
        assert_eq!(VoiceBurstPacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn oversized_frame_fails_construction() {
        assert!(matches!(
            VoiceBurstPacket::new(
                ID,
                1,
                VoiceBurstKind::Frames(vec![vec![0u8; u16::MAX as usize + 1]])
            ),
            Err(MeshError::LengthExceeded { .. })
        ));
    }
}
