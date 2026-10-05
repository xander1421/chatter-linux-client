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

/// A set of syncable message types.
///
/// Every instance is normalized at construction: bits with no known type are
/// masked off, so a garbled or newer-than-us field cannot sit in the set as
/// phantom membership. The REQUEST_SYNC byte codec lives in `tlv::request_sync`.
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
            | MessageType::VoiceFrame
            // Chunked transfers have no sync bit, and must not get one without a
            // design for it. Replaying a manifest out of the gossip archive would
            // offer a transfer whose sender is long gone and whose chunks nobody
            // holds; replaying a chunk would hand a peer one piece of a file it
            // has no manifest for. Whether a third node should be able to serve a
            // finished transfer from its archive is a real question — see the P9
            // design's open questions — but the answer is store-and-forward for
            // *files*, not a bit on this bitfield.
            | MessageType::FileManifest
            | MessageType::FileChunk
            | MessageType::FileChunkRequest => return None,
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
        // Bit 11 has no type: it must not survive construction.
        let flags = SyncTypeFlags::from_raw((1 << 11) | (1 << 1));
        assert_eq!(flags.raw(), 1 << 1);
        assert!(flags.contains(MessageType::Message));

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
}
