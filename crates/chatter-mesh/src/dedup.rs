//! Packet deduplication.
//!
//! Ported from `Utils/MessageDeduplicator.swift` (the cache) and
//! `Services/BLE/BLEReceivePipeline.swift` (`context(for:localPeerID:)`, which
//! builds the key and decides whether a packet is eligible for dedup at all).
//!
//! A flooded mesh delivers the same packet along every path that exists, so this
//! cache is what turns a flood back into a message. It is time-bounded as well as
//! size-bounded because IDs are only unique in practice, not by construction:
//! holding one forever would eventually reject a legitimate packet.
//!
//! Clock handling is the caller's job — every entry point takes `now` as Unix
//! seconds. That is the sans-IO rule, and it is also what makes expiry testable.

use std::collections::{HashMap, VecDeque};

use crate::ids::{PeerId, hex_encode};
use crate::packet::{BitchatPacket, MessageType};

use sha2::{Digest, Sha256};

/// `TransportConfig.messageDedupMaxCount`.
pub const DEFAULT_CAPACITY: usize = 1000;
/// `TransportConfig.messageDedupMaxAgeSeconds` (5 minutes).
pub const DEFAULT_TTL_SECS: u64 = 300;

/// How many bytes of the payload digest go into the key. `.prefix(4)` in Swift.
const DIGEST_PREFIX_LEN: usize = 4;

struct Entry {
    id: String,
    timestamp: u64,
}

/// Insertion-ordered, size- and age-bounded set of packet keys already handled.
///
/// bitchat calls this an LRU; it is really FIFO — a hit never refreshes an
/// entry's timestamp, so eviction order is insertion order. Ported as-is, because
/// the difference is observable: under sustained flood a hot key still ages out
/// on schedule rather than being kept alive by its own duplicates, which is what
/// bounds how long a packet ID stays un-reusable.
pub struct MessageDeduplicator {
    entries: VecDeque<Entry>,
    lookup: HashMap<String, u64>,
    max_age_secs: u64,
    max_count: usize,
}

impl MessageDeduplicator {
    /// `capacity` is clamped up to 1: a zero-capacity cache would evict every
    /// entry it just inserted and report every packet as new, which is a worse
    /// failure than ignoring the argument.
    pub fn new(capacity: usize, ttl_secs: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            lookup: HashMap::new(),
            max_age_secs: ttl_secs,
            max_count: capacity.max(1),
        }
    }

    /// The key a received packet is deduplicated under:
    /// `"{sender_hex}-{timestamp_ms}-{type}-{payload_digest}"`.
    ///
    /// The payload digest — first 4 bytes of `SHA-256(payload)`, hex — is the part
    /// that is easy to drop and fatal to drop. bitchat's post-handshake flush emits
    /// the queued message, its delivery receipt, and its read receipt back to back
    /// inside a single millisecond; sender, millisecond timestamp, and type can all
    /// collide across them, and without the digest every packet after the first is
    /// silently swallowed as a duplicate.
    pub fn dedup_key(packet: &BitchatPacket) -> String {
        let sender = PeerId::from_bytes(&packet.sender_id);
        let digest = Sha256::digest(&packet.payload);
        let digest_prefix = hex_encode(digest.iter().take(DIGEST_PREFIX_LEN));
        format!(
            "{sender}-{}-{}-{digest_prefix}",
            packet.timestamp, packet.msg_type
        )
    }

    /// Whether this packet should be deduplicated at all.
    ///
    /// Two exemptions, both from `BLEReceivePipeline.context`:
    ///
    /// - **Fragments.** Every fragment of one message shares a sender and (often) a
    ///   millisecond, and the fragment header that distinguishes them is inside the
    ///   payload — which does reach the digest, so they would mostly survive. The
    ///   assembly buffer owns fragment identity regardless, and running them
    ///   through this cache would only add a way to lose one.
    /// - **Self sync replay.** A packet with `ttl == 0` whose sender is us is our
    ///   own message coming back from a gossip-sync response we asked for. It is by
    ///   definition already in this cache, and dropping it would defeat the point
    ///   of requesting it.
    pub fn should_deduplicate(packet: &BitchatPacket, local_peer_id: &PeerId) -> bool {
        let sender = PeerId::from_bytes(&packet.sender_id);
        let allow_self_sync_replay = packet.ttl == 0 && &sender == local_peer_id;
        packet.message_type() != Some(MessageType::Fragment) && !allow_self_sync_replay
    }

    /// Checks whether `key` was already seen and records it if not.
    ///
    /// Expired entries are dropped first, so a key whose entry has aged past the
    /// TTL reports as new — and is re-recorded with the current time.
    pub fn is_duplicate(&mut self, key: &str, now_unix_secs: u64) -> bool {
        self.cleanup_old_entries(now_unix_secs);

        if self.lookup.contains_key(key) {
            return true;
        }

        self.insert(key, now_unix_secs);
        self.trim_if_needed();
        false
    }

    /// Records a key without reporting on it — for packets we originated and
    /// expect to see echoed back.
    ///
    /// Unlike `markProcessed` in Swift this also enforces the size bound. Theirs
    /// appends without trimming, which is unbounded for any caller that never
    /// calls `isDuplicate`; since eviction order is insertion order either way,
    /// trimming here changes nothing except the worst case.
    pub fn mark_processed(&mut self, key: &str, now_unix_secs: u64) {
        if self.lookup.contains_key(key) {
            return;
        }
        self.insert(key, now_unix_secs);
        self.trim_if_needed();
    }

    /// Whether a key is present, without recording it.
    pub fn contains(&self, key: &str) -> bool {
        self.lookup.contains_key(key)
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.lookup.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lookup.is_empty()
    }

    /// Drops every entry, e.g. on a panic-wipe.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.lookup.clear();
    }

    /// Age-based sweep, for callers that run maintenance on a timer instead of
    /// waiting for the next packet.
    pub fn cleanup(&mut self, now_unix_secs: u64) {
        self.cleanup_old_entries(now_unix_secs);
    }

    fn insert(&mut self, key: &str, now: u64) {
        self.entries.push_back(Entry {
            id: key.to_owned(),
            timestamp: now,
        });
        self.lookup.insert(key.to_owned(), now);
    }

    /// Removes entries older than `now - max_age`, oldest first.
    ///
    /// Timestamps are non-decreasing in insertion order for any sane clock, so
    /// stopping at the first live entry is correct. A clock that jumps backwards
    /// only delays eviction; it never evicts a live key early.
    fn cleanup_old_entries(&mut self, now: u64) {
        // Saturating rather than wrapping: at `now < max_age` — only reachable with
        // a bogus clock — the cutoff is 0 and nothing expires, which is the safe
        // direction to fail.
        let cutoff = now.saturating_sub(self.max_age_secs);
        while let Some(front) = self.entries.front() {
            if front.timestamp >= cutoff {
                break;
            }
            // `if let` rather than an unchecked pop: the borrow above already
            // proved the queue is non-empty, but this stays total either way.
            if let Some(expired) = self.entries.pop_front() {
                self.remove_from_lookup(&expired);
            }
        }
    }

    /// Evicts down to 75% of capacity once over it, matching Swift's amortization:
    /// trimming to exactly the cap would evict on every subsequent insert.
    fn trim_if_needed(&mut self) {
        if self.entries.len() <= self.max_count {
            return;
        }
        let target = self.max_count.saturating_mul(3) / 4;
        let remove_count = self.entries.len().saturating_sub(target);
        for _ in 0..remove_count {
            match self.entries.pop_front() {
                Some(victim) => self.remove_from_lookup(&victim),
                None => break,
            }
        }
    }

    /// Only forgets the key if the map still points at *this* entry. Guards the
    /// case where a key was re-inserted after its original entry expired.
    fn remove_from_lookup(&mut self, entry: &Entry) {
        if self.lookup.get(&entry.id) == Some(&entry.timestamp) {
            self.lookup.remove(&entry.id);
        }
    }
}

impl Default for MessageDeduplicator {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_TTL_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::SENDER_ID_LEN;

    fn packet(msg_type: MessageType, ts: u64, payload: &[u8]) -> BitchatPacket {
        BitchatPacket::new(
            msg_type,
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77],
            None,
            ts,
            payload.to_vec(),
            7,
        )
    }

    #[test]
    fn dedup_key_has_the_documented_shape() {
        let key = MessageDeduplicator::dedup_key(&packet(MessageType::Message, 1234, b"payload"));
        // sender-timestamp-type-sha256(payload)[..4]
        assert_eq!(key, "0011223344556677-1234-2-239f59ed");
    }

    #[test]
    fn dedup_key_uses_the_decimal_type_tag() {
        // 0x11 renders as 17, not "11" — the interpolation is of a UInt8.
        let key =
            MessageDeduplicator::dedup_key(&packet(MessageType::NoiseEncrypted, 1234, b"payload"));
        assert_eq!(key, "0011223344556677-1234-17-239f59ed");
    }

    #[test]
    fn dedup_key_digests_an_empty_payload_too() {
        let key = MessageDeduplicator::dedup_key(&packet(MessageType::Message, 0, b""));
        assert_eq!(key, "0011223344556677-0-2-e3b0c442");
    }

    #[test]
    fn dedup_key_separates_the_post_handshake_flush() {
        // The trap: message, delivered, and read receipt all leave in the same
        // millisecond from the same sender with the same type tag. Only the
        // payload digest tells them apart.
        let ts = 1_700_000_000_000;
        let message = packet(MessageType::NoiseEncrypted, ts, b"\x01hello");
        let delivered = packet(MessageType::NoiseEncrypted, ts, b"\x03msg-id");
        let read = packet(MessageType::NoiseEncrypted, ts, b"\x02msg-id");

        let mut dedup = MessageDeduplicator::default();
        for p in [&message, &delivered, &read] {
            assert!(
                !dedup.is_duplicate(&MessageDeduplicator::dedup_key(p), 0),
                "flush packet was swallowed as a duplicate"
            );
        }
        assert_eq!(dedup.len(), 3);
    }

    #[test]
    fn dedup_key_ignores_ttl_and_recipient_so_relays_collapse() {
        let original = packet(MessageType::Message, 42, b"hi");
        let relayed = BitchatPacket {
            ttl: 3,
            recipient_id: Some([0x22; SENDER_ID_LEN]),
            route: Some(vec![[0x33; SENDER_ID_LEN]]),
            ..original.clone()
        };
        assert_eq!(
            MessageDeduplicator::dedup_key(&original),
            MessageDeduplicator::dedup_key(&relayed)
        );
    }

    #[test]
    fn first_sighting_is_new_and_the_second_is_a_duplicate() {
        let mut dedup = MessageDeduplicator::default();
        assert!(!dedup.is_duplicate("k", 100));
        assert!(dedup.is_duplicate("k", 100));
        assert!(dedup.is_duplicate("k", 101));
        assert_eq!(dedup.len(), 1);
    }

    #[test]
    fn entries_expire_after_the_ttl() {
        let mut dedup = MessageDeduplicator::new(DEFAULT_CAPACITY, 300);
        assert!(!dedup.is_duplicate("k", 1_000));
        // Cutoff is `now - 300`, and expiry is strict `<`, so exactly at the
        // boundary the entry survives.
        assert!(dedup.is_duplicate("k", 1_300));
        assert!(!dedup.is_duplicate("k", 1_301));
        assert_eq!(dedup.len(), 1);
    }

    #[test]
    fn a_hit_does_not_refresh_the_entry() {
        let mut dedup = MessageDeduplicator::new(DEFAULT_CAPACITY, 100);
        assert!(!dedup.is_duplicate("k", 0));
        for t in 1..=100 {
            assert!(dedup.is_duplicate("k", t));
        }
        // Insertion time still governs: the repeated hits did not extend it.
        assert!(!dedup.is_duplicate("k", 101));
    }

    #[test]
    fn expiry_never_underflows_at_the_epoch() {
        let mut dedup = MessageDeduplicator::new(DEFAULT_CAPACITY, 300);
        assert!(!dedup.is_duplicate("k", 0));
        assert!(dedup.is_duplicate("k", 5));
    }

    #[test]
    fn capacity_evicts_to_three_quarters_oldest_first() {
        let mut dedup = MessageDeduplicator::new(8, DEFAULT_TTL_SECS);
        for i in 0..8 {
            assert!(!dedup.is_duplicate(&format!("k{i}"), 0));
        }
        assert_eq!(dedup.len(), 8);

        // The ninth insert overflows: 9 active, target 6, so 3 are evicted.
        assert!(!dedup.is_duplicate("k8", 0));
        assert_eq!(dedup.len(), 6);
        for i in 0..3 {
            assert!(
                !dedup.contains(&format!("k{i}")),
                "k{i} should have been evicted"
            );
        }
        for i in 3..=8 {
            assert!(
                dedup.contains(&format!("k{i}")),
                "k{i} should have survived"
            );
        }
    }

    #[test]
    fn stays_bounded_under_sustained_flood() {
        let mut dedup = MessageDeduplicator::new(DEFAULT_CAPACITY, DEFAULT_TTL_SECS);
        for i in 0..10_000 {
            dedup.is_duplicate(&format!("k{i}"), 0);
        }
        assert!(dedup.len() <= DEFAULT_CAPACITY);
        assert!(dedup.entries.len() <= DEFAULT_CAPACITY);
    }

    #[test]
    fn mark_processed_records_without_reporting_and_stays_bounded() {
        let mut dedup = MessageDeduplicator::new(4, DEFAULT_TTL_SECS);
        dedup.mark_processed("k", 0);
        assert!(dedup.contains("k"));
        assert!(dedup.is_duplicate("k", 0));

        for i in 0..100 {
            dedup.mark_processed(&format!("m{i}"), 0);
        }
        assert!(dedup.len() <= 4);
    }

    #[test]
    fn mark_processed_keeps_the_original_timestamp() {
        let mut dedup = MessageDeduplicator::new(DEFAULT_CAPACITY, 100);
        dedup.mark_processed("k", 0);
        dedup.mark_processed("k", 90);
        assert_eq!(dedup.len(), 1);
        // Still governed by the first insertion, not the second call.
        assert!(!dedup.is_duplicate("k", 101));
    }

    #[test]
    fn reset_and_cleanup() {
        let mut dedup = MessageDeduplicator::new(DEFAULT_CAPACITY, 10);
        dedup.mark_processed("k", 0);
        dedup.cleanup(5);
        assert_eq!(dedup.len(), 1);
        dedup.cleanup(100);
        assert!(dedup.is_empty());

        dedup.mark_processed("k", 0);
        dedup.reset();
        assert!(dedup.is_empty());
        assert!(dedup.entries.is_empty());
    }

    #[test]
    fn fragments_are_never_deduplicated() {
        let local = PeerId::from_bytes(&[0xAA; SENDER_ID_LEN]);
        let fragment = packet(MessageType::Fragment, 1, b"chunk");
        assert!(!MessageDeduplicator::should_deduplicate(&fragment, &local));

        let message = packet(MessageType::Message, 1, b"chunk");
        assert!(MessageDeduplicator::should_deduplicate(&message, &local));
    }

    #[test]
    fn self_sync_replay_is_exempt_but_only_at_ttl_zero() {
        let sender = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];
        let local = PeerId::from_bytes(&sender);

        let mut replay = packet(MessageType::Message, 1, b"mine");
        replay.ttl = 0;
        assert!(!MessageDeduplicator::should_deduplicate(&replay, &local));

        // Same sender, non-zero ttl: an ordinary echo, which must be dropped.
        let mut echo = replay.clone();
        echo.ttl = 1;
        assert!(MessageDeduplicator::should_deduplicate(&echo, &local));

        // ttl 0 from someone else is not a self replay.
        let other = PeerId::from_bytes(&[0x99; SENDER_ID_LEN]);
        assert!(MessageDeduplicator::should_deduplicate(&replay, &other));
    }

    #[test]
    fn unknown_message_types_are_still_deduplicated() {
        let local = PeerId::from_bytes(&[0xAA; SENDER_ID_LEN]);
        let mut unknown = packet(MessageType::Message, 1, b"x");
        unknown.msg_type = 0x7F;
        assert_eq!(unknown.message_type(), None);
        assert!(MessageDeduplicator::should_deduplicate(&unknown, &local));
    }
}
