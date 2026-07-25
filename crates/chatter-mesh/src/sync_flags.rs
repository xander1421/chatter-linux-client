//! Which message types a gossip-sync round covers.
//!
//! Ported from `Sync/SyncTypeFlags.swift`. A bitfield, one bit per syncable
//! message type, carried in TLV 0x04 of REQUEST_SYNC and shared with the Android
//! client — the bit↔type table below is the interop contract, not an internal
//! detail.
//!
//! # Not every type is syncable
//!
//! Five wire types deliberately have no bit, and the omissions are policy rather
//! than oversight:
//!
//! - **Courier envelopes** are directed deposits between trusted peers. Gossiping
//!   one would spread a message its sender chose *not* to broadcast.
//! - **Ping / pong** are ephemeral directed probes; a replayed probe is a stale,
//!   unanswerable echo.
//! - **Nostr carriers** are live gateway traffic — uplinks directed, downlinks
//!   rate-budgeted — and replaying them extends their lifetime past their budget.
//! - **Voice frames** are only useful live; receivers drop stale audio anyway, so
//!   syncing it is pure airtime.
//!
//! Ask for one of these and you get a flag set that does not contain it.

use crate::packet::MessageType;

/// Wire form is 1–8 bytes; a set with bits above 8 needs no format change.
const MAX_FLAG_BYTES: usize = 8;

/// A set of syncable message types.
///
/// Every instance is normalized at construction: bits with no known type are
/// masked off. Without that, a garbled or newer-than-us field would sit in the set
/// as phantom membership that no [`contains`](Self::contains) check matches while
/// [`to_data`](Self::to_data) faithfully re-serializes it — accepted, but doing
/// nothing, and impossible to spot in a log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SyncTypeFlags(u64);

impl SyncTypeFlags {
    /// The union of every bit that maps to a message type.
    ///
    /// Derived from the table rather than written as a literal, so it tracks
    /// automatically when a type is added — exactly as the `knownTypeMask` lazy
    /// static does in Swift.
    const KNOWN_TYPE_MASK: u64 = {
        let mut mask: u64 = 0;
        let mut bit: u32 = 0;
        while bit < 64 {
            if Self::type_for_bit(bit).is_some() {
                mask |= 1u64 << bit;
            }
            bit += 1;
        }
        mask
    };

    /// The empty set.
    pub const fn new() -> Self {
        Self(0)
    }

    /// Wraps a raw bitfield, masking off every bit with no known type.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw & Self::KNOWN_TYPE_MASK)
    }

    /// Builds a set from message types, skipping any that are not syncable.
    pub fn from_types(types: &[MessageType]) -> Self {
        let mut raw: u64 = 0;
        for t in types {
            if let Some(bit) = Self::bit_index(*t) {
                raw |= 1u64 << bit;
            }
        }
        Self::from_raw(raw)
    }

    /// The masked bitfield.
    pub const fn raw(&self) -> u64 {
        self.0
    }

    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Whether this round covers `t`. Always false for a non-syncable type.
    pub fn contains(&self, t: MessageType) -> bool {
        match Self::bit_index(t) {
            Some(bit) => (self.0 & (1u64 << bit)) != 0,
            None => false,
        }
    }

    /// Adds `t`, or does nothing if `t` is not syncable.
    pub fn insert(&mut self, t: MessageType) {
        if let Some(bit) = Self::bit_index(t) {
            self.0 |= 1u64 << bit;
        }
    }

    /// Removes `t`.
    pub fn remove(&mut self, t: MessageType) {
        if let Some(bit) = Self::bit_index(t) {
            self.0 &= !(1u64 << bit);
        }
    }

    pub const fn union(self, other: Self) -> Self {
        Self::from_raw(self.0 | other.0)
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self::from_raw(self.0 & other.0)
    }

    /// Every type in the set, in bit order.
    pub fn to_message_types(&self) -> Vec<MessageType> {
        let mut types = Vec::new();
        if self.0 == 0 {
            return types;
        }
        for bit in 0..64 {
            if (self.0 & (1u64 << bit)) == 0 {
                continue;
            }
            if let Some(t) = Self::type_for_bit(bit) {
                types.push(t);
            }
        }
        types
    }

    /// Little-endian bytes with trailing zero bytes trimmed.
    ///
    /// This is what makes new bits compat-safe: bit 8 (board posts) widens the
    /// field from one byte to two inside a length-prefixed TLV, and a client that
    /// predates the bit reads the wider field, finds no type for it, and answers
    /// with the types it does know.
    ///
    /// An empty set encodes to no bytes at all. Swift returns `nil` there, and its
    /// callers omit the TLV entirely — an empty vector is the same instruction.
    pub fn to_data(&self) -> Vec<u8> {
        if self.0 == 0 {
            return Vec::new();
        }
        let mut value = self.0;
        let mut bytes: Vec<u8> = Vec::with_capacity(MAX_FLAG_BYTES);
        while value > 0 && bytes.len() < MAX_FLAG_BYTES {
            bytes.push((value & 0xFF) as u8);
            value >>= 8;
        }
        while bytes.last() == Some(&0) {
            bytes.pop();
        }
        bytes
    }

    /// Decodes 1–8 little-endian bytes.
    ///
    /// Anything else — an empty field, or one longer than the bitfield — is
    /// rejected as the empty set, where Swift returns `nil`. Callers treat both as
    /// "no usable type filter"; a partially-read length is never salvaged into a
    /// set that would then drive a sync round.
    pub fn from_data(bytes: &[u8]) -> Self {
        if bytes.is_empty() || bytes.len() > MAX_FLAG_BYTES {
            return Self(0);
        }
        let mut raw: u64 = 0;
        for (index, byte) in bytes.iter().enumerate() {
            raw |= u64::from(*byte) << (index * 8);
        }
        Self::from_raw(raw)
    }

    /// The bit for a message type, or `None` when the type is not syncable.
    pub const fn bit_index(t: MessageType) -> Option<u32> {
        Some(match t {
            MessageType::Announce => 0,
            MessageType::Message => 1,
            MessageType::Leave => 2,
            MessageType::NoiseHandshake => 3,
            MessageType::NoiseEncrypted => 4,
            MessageType::Fragment => 5,
            MessageType::RequestSync => 6,
            MessageType::FileTransfer => 7,
            MessageType::BoardPost => 8,
            MessageType::PrekeyBundle => 9,
            MessageType::GroupMessage => 10,
            MessageType::CourierEnvelope
            | MessageType::Ping
            | MessageType::Pong
            | MessageType::NostrCarrier
            | MessageType::VoiceFrame => return None,
        })
    }

    /// The message type a bit stands for, or `None` for an unassigned bit.
    pub const fn type_for_bit(bit: u32) -> Option<MessageType> {
        Some(match bit {
            0 => MessageType::Announce,
            1 => MessageType::Message,
            2 => MessageType::Leave,
            3 => MessageType::NoiseHandshake,
            4 => MessageType::NoiseEncrypted,
            5 => MessageType::Fragment,
            6 => MessageType::RequestSync,
            7 => MessageType::FileTransfer,
            8 => MessageType::BoardPost,
            9 => MessageType::PrekeyBundle,
            10 => MessageType::GroupMessage,
            _ => return None,
        })
    }

    /// `"message+fragment"`-style rendering for logs.
    ///
    /// bitchat added this because periodic sync rounds otherwise log identical
    /// lines and read as duplicate sends — a misdiagnosis they made twice.
    pub fn log_description(&self) -> String {
        let types = self.to_message_types();
        if types.is_empty() {
            return "none".to_string();
        }
        types
            .iter()
            .map(|t| type_name(*t))
            .collect::<Vec<_>>()
            .join("+")
    }
}

/// Lowercase-initial names matching Swift's `String(describing:)` on the enum
/// case, so log lines are diffable against theirs.
const fn type_name(t: MessageType) -> &'static str {
    match t {
        MessageType::Announce => "announce",
        MessageType::Message => "message",
        MessageType::Leave => "leave",
        MessageType::CourierEnvelope => "courierEnvelope",
        MessageType::NoiseHandshake => "noiseHandshake",
        MessageType::NoiseEncrypted => "noiseEncrypted",
        MessageType::Fragment => "fragment",
        MessageType::RequestSync => "requestSync",
        MessageType::FileTransfer => "fileTransfer",
        MessageType::BoardPost => "boardPost",
        MessageType::PrekeyBundle => "prekeyBundle",
        MessageType::GroupMessage => "groupMessage",
        MessageType::Ping => "ping",
        MessageType::Pong => "pong",
        MessageType::NostrCarrier => "nostrCarrier",
        MessageType::VoiceFrame => "voiceFrame",
    }
}

/// The named sets Swift exposes as statics.
impl SyncTypeFlags {
    pub const ANNOUNCE: Self = Self::from_raw(1 << 0);
    pub const MESSAGE: Self = Self::from_raw(1 << 1);
    pub const FRAGMENT: Self = Self::from_raw(1 << 5);
    pub const FILE_TRANSFER: Self = Self::from_raw(1 << 7);
    pub const BOARD: Self = Self::from_raw(1 << 8);
    pub const PREKEY_BUNDLE: Self = Self::from_raw(1 << 9);
    pub const GROUP_MESSAGE: Self = Self::from_raw(1 << 10);
    /// `announce | message` — the default periodic round.
    pub const PUBLIC_MESSAGES: Self = Self::from_raw((1 << 0) | (1 << 1));
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNCABLE: [MessageType; 11] = [
        MessageType::Announce,
        MessageType::Message,
        MessageType::Leave,
        MessageType::NoiseHandshake,
        MessageType::NoiseEncrypted,
        MessageType::Fragment,
        MessageType::RequestSync,
        MessageType::FileTransfer,
        MessageType::BoardPost,
        MessageType::PrekeyBundle,
        MessageType::GroupMessage,
    ];

    const NOT_SYNCABLE: [MessageType; 5] = [
        MessageType::CourierEnvelope,
        MessageType::Ping,
        MessageType::Pong,
        MessageType::NostrCarrier,
        MessageType::VoiceFrame,
    ];

    #[test]
    fn bit_table_matches_the_android_mapping() {
        let expected = [
            (MessageType::Announce, 0),
            (MessageType::Message, 1),
            (MessageType::Leave, 2),
            (MessageType::NoiseHandshake, 3),
            (MessageType::NoiseEncrypted, 4),
            (MessageType::Fragment, 5),
            (MessageType::RequestSync, 6),
            (MessageType::FileTransfer, 7),
            (MessageType::BoardPost, 8),
            (MessageType::PrekeyBundle, 9),
            (MessageType::GroupMessage, 10),
        ];
        for (t, bit) in expected {
            assert_eq!(SyncTypeFlags::bit_index(t), Some(bit));
            assert_eq!(SyncTypeFlags::type_for_bit(bit), Some(t));
        }
    }

    #[test]
    fn the_two_tables_are_inverses() {
        for t in SYNCABLE {
            let bit = SyncTypeFlags::bit_index(t).expect("syncable type has a bit");
            assert_eq!(SyncTypeFlags::type_for_bit(bit), Some(t));
        }
        for bit in 0..64u32 {
            if let Some(t) = SyncTypeFlags::type_for_bit(bit) {
                assert_eq!(SyncTypeFlags::bit_index(t), Some(bit));
            }
        }
    }

    #[test]
    fn non_syncable_types_have_no_bit() {
        for t in NOT_SYNCABLE {
            assert_eq!(SyncTypeFlags::bit_index(t), None);
            let mut flags = SyncTypeFlags::from_raw(u64::MAX);
            assert!(!flags.contains(t));
            flags.insert(t);
            assert!(!flags.contains(t));
        }
    }

    #[test]
    fn known_mask_covers_exactly_bits_zero_through_ten() {
        assert_eq!(SyncTypeFlags::KNOWN_TYPE_MASK, 0b111_1111_1111);
        assert_eq!(SyncTypeFlags::from_raw(u64::MAX).raw(), 0b111_1111_1111);
    }

    #[test]
    fn from_raw_masks_phantom_bits() {
        // Bit 11 has no type: it must not survive construction, or it would
        // round-trip through to_data while matching no contains check.
        let flags = SyncTypeFlags::from_raw((1 << 11) | (1 << 1));
        assert_eq!(flags.raw(), 1 << 1);
        assert!(flags.contains(MessageType::Message));
        assert_eq!(flags.to_data(), vec![0b10]);

        assert_eq!(SyncTypeFlags::from_raw(1 << 63).raw(), 0);
        assert!(SyncTypeFlags::from_raw(1 << 63).is_empty());
    }

    #[test]
    fn insert_and_contains() {
        let mut flags = SyncTypeFlags::new();
        assert!(flags.is_empty());
        flags.insert(MessageType::Announce);
        flags.insert(MessageType::GroupMessage);
        assert!(flags.contains(MessageType::Announce));
        assert!(flags.contains(MessageType::GroupMessage));
        assert!(!flags.contains(MessageType::Message));
        assert_eq!(flags.raw(), (1 << 0) | (1 << 10));

        flags.remove(MessageType::Announce);
        assert!(!flags.contains(MessageType::Announce));
    }

    #[test]
    fn one_byte_wire_form_for_low_bits() {
        let flags = SyncTypeFlags::from_types(&[MessageType::Announce, MessageType::Message]);
        assert_eq!(flags, SyncTypeFlags::PUBLIC_MESSAGES);
        assert_eq!(flags.to_data(), vec![0b0000_0011]);
        assert_eq!(SyncTypeFlags::from_data(&[0b0000_0011]), flags);
    }

    #[test]
    fn bit_eight_widens_the_field_to_two_bytes() {
        // The compat case called out in the Swift: board posts spill into a
        // second byte, and old clients must survive reading it.
        let flags = SyncTypeFlags::from_types(&[MessageType::BoardPost]);
        assert_eq!(flags.to_data(), vec![0x00, 0x01]);
        assert_eq!(SyncTypeFlags::from_data(&[0x00, 0x01]), flags);

        let group = SyncTypeFlags::from_types(&[MessageType::GroupMessage]);
        assert_eq!(group.to_data(), vec![0x00, 0x04]);
        assert_eq!(SyncTypeFlags::from_data(&[0x00, 0x04]), group);
    }

    #[test]
    fn encoding_is_little_endian_with_trailing_zeroes_trimmed() {
        let all = SyncTypeFlags::from_types(&SYNCABLE);
        assert_eq!(all.raw(), 0b111_1111_1111);
        assert_eq!(all.to_data(), vec![0xFF, 0x07]);
        assert_eq!(SyncTypeFlags::from_data(&all.to_data()), all);
    }

    #[test]
    fn empty_set_encodes_to_nothing() {
        assert!(SyncTypeFlags::new().to_data().is_empty());
        assert!(SyncTypeFlags::from_raw(0).to_data().is_empty());
    }

    #[test]
    fn decode_rejects_bad_lengths() {
        assert!(SyncTypeFlags::from_data(&[]).is_empty());
        assert!(SyncTypeFlags::from_data(&[0xFF; 9]).is_empty());
        // Exactly 8 bytes is legal, and the high bytes mask away to nothing.
        assert_eq!(SyncTypeFlags::from_data(&[0xFF; 8]).raw(), 0b111_1111_1111);
    }

    #[test]
    fn decode_tolerates_bits_a_newer_peer_added() {
        // A peer that syncs a type we do not know sets a bit we have no entry
        // for. We must answer with the types we do know, not choke.
        let wire = [0b0000_0011u8, 0x00, 0xFF];
        let flags = SyncTypeFlags::from_data(&wire);
        assert!(flags.contains(MessageType::Announce));
        assert!(flags.contains(MessageType::Message));
        assert_eq!(flags.raw(), 0b11);
    }

    #[test]
    fn round_trips_every_representable_set() {
        for raw in 0..=SyncTypeFlags::KNOWN_TYPE_MASK {
            let flags = SyncTypeFlags::from_raw(raw);
            let data = flags.to_data();
            assert!(data.len() <= MAX_FLAG_BYTES);
            assert_eq!(SyncTypeFlags::from_data(&data), flags, "raw {raw:#x}");
        }
    }

    #[test]
    fn to_message_types_is_in_bit_order() {
        let flags = SyncTypeFlags::from_types(&[
            MessageType::GroupMessage,
            MessageType::Announce,
            MessageType::Fragment,
        ]);
        assert_eq!(
            flags.to_message_types(),
            vec![
                MessageType::Announce,
                MessageType::Fragment,
                MessageType::GroupMessage
            ]
        );
        assert!(SyncTypeFlags::new().to_message_types().is_empty());
    }

    #[test]
    fn from_types_skips_non_syncable_types() {
        let flags = SyncTypeFlags::from_types(&[MessageType::VoiceFrame, MessageType::Message]);
        assert_eq!(flags, SyncTypeFlags::MESSAGE);
    }

    #[test]
    fn union_and_intersection() {
        let a = SyncTypeFlags::from_types(&[MessageType::Announce, MessageType::Message]);
        let b = SyncTypeFlags::from_types(&[MessageType::Message, MessageType::Fragment]);
        assert_eq!(a.union(b).to_message_types().len(), 3);
        assert_eq!(a.intersection(b), SyncTypeFlags::MESSAGE);
        assert!(a.intersection(SyncTypeFlags::new()).is_empty());
    }

    #[test]
    fn named_sets_match_their_types() {
        assert_eq!(
            SyncTypeFlags::ANNOUNCE,
            SyncTypeFlags::from_types(&[MessageType::Announce])
        );
        assert_eq!(
            SyncTypeFlags::MESSAGE,
            SyncTypeFlags::from_types(&[MessageType::Message])
        );
        assert_eq!(
            SyncTypeFlags::FRAGMENT,
            SyncTypeFlags::from_types(&[MessageType::Fragment])
        );
        assert_eq!(
            SyncTypeFlags::FILE_TRANSFER,
            SyncTypeFlags::from_types(&[MessageType::FileTransfer])
        );
        assert_eq!(
            SyncTypeFlags::BOARD,
            SyncTypeFlags::from_types(&[MessageType::BoardPost])
        );
        assert_eq!(
            SyncTypeFlags::PREKEY_BUNDLE,
            SyncTypeFlags::from_types(&[MessageType::PrekeyBundle])
        );
        assert_eq!(
            SyncTypeFlags::GROUP_MESSAGE,
            SyncTypeFlags::from_types(&[MessageType::GroupMessage])
        );
        assert_eq!(
            SyncTypeFlags::PUBLIC_MESSAGES,
            SyncTypeFlags::from_types(&[MessageType::Announce, MessageType::Message])
        );
    }

    #[test]
    fn log_description_joins_type_names() {
        assert_eq!(SyncTypeFlags::new().log_description(), "none");
        assert_eq!(
            SyncTypeFlags::from_types(&[MessageType::Message, MessageType::Fragment])
                .log_description(),
            "message+fragment"
        );
    }
}
