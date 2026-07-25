//! The first thing hostile radio input meets.
//!
//! Ported from `Services/BLE/BLEIngressPacketGuard.swift`,
//! `BLEPacketFreshnessPolicy.swift`, and `BLESelfBroadcastTracker.swift`, with the
//! link-binding checks that `BLEIngressLinkRegistry.packetContext` performs on the
//! guard's behalf folded in — they are part of the accept/reject decision, and
//! splitting them across two crates' worth of files is how one of them gets skipped.
//!
//! Everything here runs *before* any signature is checked, on bytes that arrived
//! from a stranger. So:
//!
//! - no panics, ever: every subtraction saturates and every length is checked;
//! - no clock: `now_unix_millis` is a parameter, so a test can put a packet two
//!   minutes in the future without waiting;
//! - no storage invented for state this crate does not own: whether a sync response
//!   was solicited is an injected predicate, because the request book lives in the
//!   sync manager above us.
//!
//! What the guard can and cannot prove is worth being precise about. It can prove a
//! packet is not ours, that its claimed sender matches the identity already bound to
//! the link for the packet types where that must hold, and that its timestamp is
//! plausible. It **cannot** prove the sender is who they say they are — only a
//! verified signature does that (see [`crate::announce`]). In particular
//! [`IngressAccept::direct_announce_hint`] rides on the TTL byte, which is excluded
//! from the signature preimage so relays can decrement it, and is therefore forgeable
//! by anyone. Treat it as a hint and nothing more.

use std::collections::HashMap;

use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{BROADCAST_RECIPIENT, MessageType, SENDER_ID_LEN};
use chatter_mesh::{BitchatPacket, relay::MESSAGE_TTL_DEFAULT};

/// `BLEPacketFreshnessPolicy.defaultMaxAgeSeconds` — 15 minutes.
///
/// The *staleness* window, used one-sidedly: a packet older than this is dropped,
/// a packet from the future never is. It is deliberately wider than
/// [`DEFAULT_MAX_TIMESTAMP_SKEW_MILLIS`], because it gates announce replay across a
/// multi-hop mesh where a relayed announce genuinely can be minutes old.
pub const DEFAULT_MAX_AGE_MILLIS: u64 = 900_000;

/// `BLEIngressPacketGuard`'s `maxTimestampSkewMs` — 2 minutes, symmetric.
///
/// The *skew* window, applied to every non-RSR packet on ingress in both
/// directions. Two minutes is roughly the widest clock disagreement between two
/// phones that have each synced to network time at some point; wider than that and
/// a captured packet stays replayable for as long as the window.
pub const DEFAULT_MAX_TIMESTAMP_SKEW_MILLIS: u64 = 120_000;

/// `TransportConfig.messageDedupMaxAgeSeconds` — how long a self-broadcast record
/// is kept before pruning. Matches the message deduplicator's window, since the two
/// are pruned for the same reason: a relayed copy of our own packet can arrive late.
pub const SELF_BROADCAST_RETENTION_MILLIS: u64 = 300_000;

/// Freshness arithmetic, shared by the ingress guard and the announce preflight.
///
/// Ported from `BLEPacketFreshnessPolicy`. Two windows with two different shapes
/// live here; conflating them is the mistake this type exists to make hard.
pub struct PacketFreshnessPolicy;

impl PacketFreshnessPolicy {
    /// True when a recipient field means "everyone".
    ///
    /// Note the asymmetry with [`BitchatPacket::is_broadcast`]: there, an absent
    /// recipient is *not* broadcast (it is unaddressed). Here it is, because Swift's
    /// `isBroadcastRecipient(nil)` returns `true` — freshness treats "no recipient"
    /// as public traffic. Both are correct for their own question, so the two are
    /// kept apart rather than unified.
    pub fn is_broadcast_recipient(recipient: Option<&[u8; SENDER_ID_LEN]>) -> bool {
        match recipient {
            None => true,
            Some(id) => *id == BROADCAST_RECIPIENT,
        }
    }

    /// One-sided staleness: is this packet older than `max_age_millis`?
    ///
    /// A future timestamp is never stale, mirroring Swift. That is not sloppiness —
    /// this window guards announce replay, and the symmetric check that catches
    /// implausible future timestamps is [`Self::is_within_skew`], applied to the same
    /// packet on ingress.
    ///
    /// The `now < max_age` guard reproduces Swift's `guard nowMilliseconds >=
    /// maxAgeMilliseconds else { return false }`. It only fires for clocks set near
    /// the epoch, where nothing can be judged stale.
    pub fn is_stale(timestamp_millis: u64, now_unix_millis: u64, max_age_millis: u64) -> bool {
        if now_unix_millis < max_age_millis {
            return false;
        }
        timestamp_millis < now_unix_millis - max_age_millis
    }

    /// How far in the past this timestamp is, or `0` if it is in the future.
    pub fn age_millis(timestamp_millis: u64, now_unix_millis: u64) -> u64 {
        now_unix_millis.saturating_sub(timestamp_millis)
    }

    /// Absolute distance between the timestamp and now, in either direction.
    pub fn skew_millis(timestamp_millis: u64, now_unix_millis: u64) -> u64 {
        timestamp_millis.abs_diff(now_unix_millis)
    }

    /// Symmetric freshness: within `max_skew_millis` of now, past or future.
    pub fn is_within_skew(
        timestamp_millis: u64,
        now_unix_millis: u64,
        max_skew_millis: u64,
    ) -> bool {
        Self::skew_millis(timestamp_millis, now_unix_millis) <= max_skew_millis
    }
}

/// Identifies one packet for self-broadcast purposes.
///
/// `BLESelfBroadcastTracker.dedupID` builds `"{senderHex}-{timestamp}-{type}"`; the
/// tuple is the same key without the string formatting. Deliberately *not*
/// [`BitchatPacket::packet_id`], which also hashes the payload: this key must match
/// across a relay that rewrote nothing but the TTL, and it is the same key
/// `BLEService` pre-marks in the message deduplicator when it sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SelfBroadcastKey {
    pub sender_id: [u8; SENDER_ID_LEN],
    pub timestamp: u64,
    pub msg_type: u8,
}

impl SelfBroadcastKey {
    pub fn for_packet(packet: &BitchatPacket) -> Self {
        Self {
            sender_id: packet.sender_id,
            timestamp: packet.timestamp,
            msg_type: packet.msg_type,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelfBroadcastEntry {
    message_id: String,
    sent_at_millis: u64,
}

/// Remembers packets we originated, so their echoes can be recognised.
///
/// Ported from `BLESelfBroadcastTracker`. A mesh relays broadcasts back toward
/// their origin — that is flooding working as designed — so our own message returns
/// to us on someone else's link. Reprocessing it would duplicate the message in the
/// UI at best, and at worst let a relay replay our own traffic at us indefinitely.
///
/// The stored message ID is what makes this more than suppression: when a self copy
/// does surface, the caller can reconcile it with the local message it already
/// displayed instead of inventing a second one.
#[derive(Clone, Debug, Default)]
pub struct SelfBroadcastTracker {
    entries: HashMap<SelfBroadcastKey, SelfBroadcastEntry>,
}

impl SelfBroadcastTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Records a packet we just sent, keyed so its echo will match.
    pub fn record(
        &mut self,
        message_id: impl Into<String>,
        packet: &BitchatPacket,
        sent_at_millis: u64,
    ) {
        self.entries.insert(
            SelfBroadcastKey::for_packet(packet),
            SelfBroadcastEntry {
                message_id: message_id.into(),
                sent_at_millis,
            },
        );
    }

    /// True when this packet is an echo of something we sent.
    pub fn contains(&self, packet: &BitchatPacket) -> bool {
        self.entries.contains_key(&SelfBroadcastKey::for_packet(packet))
    }

    /// The local message ID for an echo, without consuming the record.
    pub fn message_id(&self, packet: &BitchatPacket) -> Option<&str> {
        self.entries
            .get(&SelfBroadcastKey::for_packet(packet))
            .map(|entry| entry.message_id.as_str())
    }

    /// The local message ID for an echo, consuming the record.
    ///
    /// Consuming is the point: the second echo of the same packet finds nothing and
    /// is handled as an ordinary duplicate rather than re-resolving to a message the
    /// caller has already reconciled.
    pub fn take_message_id(&mut self, packet: &BitchatPacket) -> Option<String> {
        self.entries
            .remove(&SelfBroadcastKey::for_packet(packet))
            .map(|entry| entry.message_id)
    }

    /// Drops records sent before `cutoff_millis`. Retention is bounded because this
    /// map grows with every message we send and nothing else would ever shrink it.
    pub fn prune(&mut self, cutoff_millis: u64) {
        if self.entries.is_empty() {
            return;
        }
        self.entries
            .retain(|_, entry| entry.sent_at_millis >= cutoff_millis);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Why a packet was refused at the door.
///
/// Every variant carries what a log line would need, because a rejection with no
/// attribution is indistinguishable from a bug when the mesh misbehaves in the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngressRejection {
    /// The packet claims *our* sender ID. Either a relay handed our own broadcast
    /// back to us or someone is impersonating us on this link; neither is worth
    /// processing.
    SelfLoopback { msg_type: u8 },

    /// The packet matches something we sent, by sender, timestamp, and type.
    ///
    /// `message_id` is the local message it echoes, when we still hold the record —
    /// the caller reconciles against that rather than creating a duplicate.
    SelfBroadcastEcho {
        msg_type: u8,
        message_id: Option<String>,
    },

    /// A packet type that is never relayed arrived on a link already bound to a
    /// different peer. `RequestSync` elicits a full store replay addressed to
    /// whoever the sender claims to be, so accepting a mismatched claim here would
    /// let anyone with a link drain another peer's inbox.
    DirectSenderMismatch { bound: PeerId, claimed: PeerId },

    /// A sync response that answers no request we sent.
    ///
    /// The RSR flag exempts a packet from the freshness check. Honouring that flag
    /// on an unsolicited packet would hand every peer an unlimited replay window for
    /// arbitrarily old traffic, so the exemption is only ever granted to a response
    /// we asked for.
    UnsolicitedSyncResponse { peer: PeerId },

    /// The timestamp is further from our clock than the skew window allows, in
    /// either direction.
    TimestampSkew {
        peer: PeerId,
        skew_millis: u64,
        max_skew_millis: u64,
    },
}

impl core::fmt::Display for IngressRejection {
    /// Rejections are the main diagnostic when a mesh link looks connected but
    /// carries nothing, so they need to render.
    ///
    /// Peer IDs are included because a rejection is meaningless without knowing who
    /// it applies to; nothing here exposes packet contents.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SelfLoopback { msg_type } => {
                write!(f, "packet type {msg_type} claims our own sender ID")
            }
            Self::SelfBroadcastEcho {
                msg_type,
                message_id,
            } => match message_id {
                Some(id) => write!(f, "echo of our own type-{msg_type} packet ({id})"),
                None => write!(f, "echo of our own type-{msg_type} packet"),
            },
            Self::DirectSenderMismatch { bound, claimed } => write!(
                f,
                "never-relayed packet claims {claimed} on a link bound to {bound}"
            ),
            Self::UnsolicitedSyncResponse { peer } => {
                write!(f, "sync response from {peer} answering no request of ours")
            }
            Self::TimestampSkew {
                peer,
                skew_millis,
                max_skew_millis,
            } => write!(
                f,
                "timestamp from {peer} is {skew_millis}ms off, limit {max_skew_millis}ms"
            ),
        }
    }
}

/// A packet that survived every check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressAccept {
    /// The peer this packet physically came from: the link's bound identity when
    /// there is one, otherwise the claimed sender. This is the one to hold a link
    /// accountable with.
    pub received_from: PeerId,

    /// The peer whose rules this packet is validated against: the claimed sender
    /// normally, the link's bound identity for a sync response — a response is only
    /// meaningful from the peer we asked, no matter whose ID is stamped on it.
    pub validation_peer: PeerId,

    /// **Unauthenticated hint.** An announce whose TTL still equals the origination
    /// TTL, which usually means it came straight from its author. The TTL is
    /// excluded from the signature preimage so relays can decrement it, so anyone
    /// can set it back to 7 and look "direct". Never treat this as proof of
    /// adjacency; it may only bias decisions that are separately authenticated.
    pub direct_announce_hint: bool,

    /// Whether the freshness check was skipped because this is a solicited sync
    /// response. Surfaced so a caller applying its own age policy knows the
    /// timestamp was never vetted.
    pub freshness_exempt: bool,
}

/// The inbound checks, in one decision.
///
/// Ported from `BLEIngressPacketGuard.evaluate` plus the `packetContext` half of
/// `BLEIngressLinkRegistry`. Order matters and is preserved from Swift: link-binding
/// rejections are decided before timestamp validation, so a spoofing attempt is
/// reported as spoofing rather than being masked by whatever its clock said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IngressGuard {
    /// TTL an announce carries at origination. Used only to compute
    /// [`IngressAccept::direct_announce_hint`].
    pub direct_announce_ttl: u8,
    /// Symmetric freshness window for non-RSR packets.
    pub max_timestamp_skew_millis: u64,
}

impl Default for IngressGuard {
    fn default() -> Self {
        Self {
            direct_announce_ttl: MESSAGE_TTL_DEFAULT,
            max_timestamp_skew_millis: DEFAULT_MAX_TIMESTAMP_SKEW_MILLIS,
        }
    }
}

/// One inbound packet and everything known about where it arrived.
#[derive(Clone, Copy, Debug)]
pub struct IngressInput<'a> {
    pub packet: &'a BitchatPacket,
    /// The sender ID the packet claims. Unverified by definition.
    pub claimed_sender: &'a PeerId,
    /// The identity already bound to this link, if a verified announce has bound one.
    pub bound_peer: Option<&'a PeerId>,
    /// Our own peer ID.
    pub local_peer: &'a PeerId,
    pub now_unix_millis: u64,
}

impl IngressGuard {
    pub fn new(direct_announce_ttl: u8, max_timestamp_skew_millis: u64) -> Self {
        Self {
            direct_announce_ttl,
            max_timestamp_skew_millis,
        }
    }

    /// True for the one packet shape that is legitimately authored by us and still
    /// arrives at us: a link-local sync response we generated, TTL 0 so it is never
    /// relayed. Swift's `isSelfAuthoredSyncResponse`. Everything else bearing our
    /// sender ID is a loopback or an impersonation.
    pub fn is_self_authored_sync_response(packet: &BitchatPacket) -> bool {
        packet.is_rsr && packet.ttl == 0
    }

    /// True for packet types that are never relayed, so the claimed sender must be
    /// the peer bound to the link. Swift's `requiresDirectSenderBinding`.
    pub fn requires_direct_sender_binding(packet: &BitchatPacket) -> bool {
        packet.msg_type == MessageType::RequestSync.as_u8()
    }

    /// **Unauthenticated.** An announce still carrying the origination TTL.
    pub fn is_direct_announce_hint(&self, packet: &BitchatPacket) -> bool {
        packet.msg_type == MessageType::Announce.as_u8() && packet.ttl == self.direct_announce_ttl
    }

    /// Accept or reject one inbound packet.
    ///
    /// `is_solicited_sync_response` answers "did we send this peer a sync request
    /// that is still outstanding?". It is injected because this crate deliberately
    /// keeps no request book — inventing storage for it here would put the answer in
    /// two places and let them disagree, and a disagreement means an attacker gets
    /// the freshness exemption for free.
    pub fn evaluate(
        &self,
        input: IngressInput<'_>,
        self_broadcasts: &SelfBroadcastTracker,
        is_solicited_sync_response: impl FnOnce(&PeerId) -> bool,
    ) -> Result<IngressAccept, IngressRejection> {
        let packet = input.packet;

        // Our own traffic, coming back. A self-authored TTL-0 sync response is the
        // sole exception, and it must be tested first: it legitimately carries our
        // sender ID, and both self checks below would otherwise drop it.
        if !Self::is_self_authored_sync_response(packet) {
            if self_broadcasts.contains(packet) {
                return Err(IngressRejection::SelfBroadcastEcho {
                    msg_type: packet.msg_type,
                    message_id: self_broadcasts.message_id(packet).map(str::to_owned),
                });
            }
            if input.claimed_sender == input.local_peer {
                return Err(IngressRejection::SelfLoopback {
                    msg_type: packet.msg_type,
                });
            }
        }

        let direct_announce_hint = self.is_direct_announce_hint(packet);

        // A link already bound to someone else, with a packet claiming a different
        // sender.
        let mismatched_binding = match input.bound_peer {
            Some(bound) => bound != input.claimed_sender,
            None => false,
        };
        if mismatched_binding {
            let bound = input.bound_peer.unwrap_or(input.claimed_sender);

            if Self::requires_direct_sender_binding(packet) {
                return Err(IngressRejection::DirectSenderMismatch {
                    bound: bound.clone(),
                    claimed: input.claimed_sender.clone(),
                });
            }

            // A direct-looking announce claiming a new sender on a bound link is
            // either a peer-ID rotation on a connection that outlived the old ID, or
            // a replay with its TTL restored. The two are indistinguishable here,
            // because the TTL is not signed. Attribute it to the claimed sender and
            // let it through — announces are self-authenticating, and only a
            // signature-verified announce may actually rebind the link (see
            // [`crate::announce`]). It is not a freshness bypass: the same payload
            // validation runs before it is accepted.
            if direct_announce_hint {
                let peer = input.claimed_sender.clone();
                self.validate_payload(
                    packet,
                    &peer,
                    input.now_unix_millis,
                    is_solicited_sync_response,
                )?;
                return Ok(IngressAccept {
                    received_from: peer.clone(),
                    validation_peer: peer,
                    direct_announce_hint,
                    freshness_exempt: packet.is_rsr,
                });
            }
        }

        let received_from = input.bound_peer.unwrap_or(input.claimed_sender).clone();
        // A sync response is validated against the link's peer, not the ID stamped
        // on it: we asked a specific peer, and only that peer's answer is solicited.
        let validation_peer = if packet.is_rsr {
            received_from.clone()
        } else {
            input.claimed_sender.clone()
        };

        self.validate_payload(
            packet,
            &validation_peer,
            input.now_unix_millis,
            is_solicited_sync_response,
        )?;

        Ok(IngressAccept {
            received_from,
            validation_peer,
            direct_announce_hint,
            freshness_exempt: packet.is_rsr,
        })
    }

    /// Freshness alone, for callers that already resolved the peer.
    ///
    /// Swift's `validatePayload`, and the second entry point `BLEService` uses on
    /// paths where the link binding was settled earlier.
    ///
    /// The RSR branch is the security-critical one. `is_rsr` is a bit an attacker
    /// controls; what they cannot control is whether we have an outstanding request
    /// to them. Granting the exemption on the flag alone would make every captured
    /// packet replayable forever.
    pub fn validate_payload(
        &self,
        packet: &BitchatPacket,
        peer: &PeerId,
        now_unix_millis: u64,
        is_solicited_sync_response: impl FnOnce(&PeerId) -> bool,
    ) -> Result<(), IngressRejection> {
        if packet.is_rsr {
            if is_solicited_sync_response(peer) {
                return Ok(());
            }
            return Err(IngressRejection::UnsolicitedSyncResponse { peer: peer.clone() });
        }

        let skew = PacketFreshnessPolicy::skew_millis(packet.timestamp, now_unix_millis);
        if skew > self.max_timestamp_skew_millis {
            return Err(IngressRejection::TimestampSkew {
                peer: peer.clone(),
                skew_millis: skew,
                max_skew_millis: self.max_timestamp_skew_millis,
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh::packet::SIGNATURE_LEN;

    const LOCAL: [u8; SENDER_ID_LEN] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];
    const BOUND: [u8; SENDER_ID_LEN] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
    const SENDER: [u8; SENDER_ID_LEN] = [0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];

    fn peer(id: [u8; SENDER_ID_LEN]) -> PeerId {
        PeerId::from_bytes(&id)
    }

    fn packet(
        msg_type: MessageType,
        sender: [u8; SENDER_ID_LEN],
        timestamp: u64,
        ttl: u8,
    ) -> BitchatPacket {
        BitchatPacket::new(msg_type, sender, None, timestamp, vec![1, 2, 3], ttl)
    }

    fn message(sender: [u8; SENDER_ID_LEN], timestamp: u64) -> BitchatPacket {
        packet(MessageType::Message, sender, timestamp, 3)
    }

    fn input<'a>(
        pkt: &'a BitchatPacket,
        claimed: &'a PeerId,
        bound: Option<&'a PeerId>,
        local: &'a PeerId,
        now: u64,
    ) -> IngressInput<'a> {
        IngressInput {
            packet: pkt,
            claimed_sender: claimed,
            bound_peer: bound,
            local_peer: local,
            now_unix_millis: now,
        }
    }

    // --- freshness policy ------------------------------------------------

    #[test]
    fn broadcast_recipient_matches_swift_including_the_nil_case() {
        assert!(PacketFreshnessPolicy::is_broadcast_recipient(None));
        assert!(PacketFreshnessPolicy::is_broadcast_recipient(Some(
            &BROADCAST_RECIPIENT
        )));
        assert!(!PacketFreshnessPolicy::is_broadcast_recipient(Some(&[
            0, 1, 2, 3, 4, 5, 6, 7
        ])));
    }

    #[test]
    fn staleness_is_one_sided_and_lands_exactly_on_the_window() {
        let now = 1_000_000;
        let window = DEFAULT_MAX_AGE_MILLIS;

        // Exactly at the boundary is not stale: Swift uses a strict `<`.
        assert!(!PacketFreshnessPolicy::is_stale(now - window, now, window));
        assert!(PacketFreshnessPolicy::is_stale(now - window - 1, now, window));
        // The future is never stale by this policy — the skew check catches it.
        assert!(!PacketFreshnessPolicy::is_stale(
            now + 10_000_000,
            now,
            window
        ));
    }

    #[test]
    fn staleness_never_underflows_near_the_epoch() {
        // now < max_age: Swift's guard returns false rather than wrapping.
        assert!(!PacketFreshnessPolicy::is_stale(0, 1_000, 900_000));
        assert!(!PacketFreshnessPolicy::is_stale(u64::MAX, 0, 900_000));
    }

    #[test]
    fn age_is_zero_for_future_timestamps_and_skew_is_not() {
        let now = 1_000_000;
        assert_eq!(PacketFreshnessPolicy::age_millis(now + 10_000, now), 0);
        assert_eq!(PacketFreshnessPolicy::age_millis(now - 10_000, now), 10_000);
        assert_eq!(PacketFreshnessPolicy::skew_millis(now + 10_000, now), 10_000);
        assert_eq!(PacketFreshnessPolicy::skew_millis(now - 10_000, now), 10_000);
    }

    #[test]
    fn skew_window_accepts_inside_and_rejects_outside_on_both_sides() {
        let now = 10_000_000;
        let max = DEFAULT_MAX_TIMESTAMP_SKEW_MILLIS;

        assert!(PacketFreshnessPolicy::is_within_skew(now, now, max));
        // Exactly on the boundary is accepted, both directions: Swift's `<=`.
        assert!(PacketFreshnessPolicy::is_within_skew(now - max, now, max));
        assert!(PacketFreshnessPolicy::is_within_skew(now + max, now, max));
        // One millisecond past it is not, both directions.
        assert!(!PacketFreshnessPolicy::is_within_skew(now - max - 1, now, max));
        assert!(!PacketFreshnessPolicy::is_within_skew(now + max + 1, now, max));
    }

    #[test]
    fn guard_accepts_inside_the_freshness_window_and_rejects_outside_it_both_ways() {
        let guard = IngressGuard::default();
        let now = 10_000_000;
        let max = DEFAULT_MAX_TIMESTAMP_SKEW_MILLIS;
        let sender = peer(SENDER);
        let local = peer(LOCAL);

        for timestamp in [now, now - max, now + max] {
            let pkt = message(SENDER, timestamp);
            assert!(
                guard
                    .evaluate(
                        input(&pkt, &sender, None, &local, now),
                        &SelfBroadcastTracker::new(),
                        |_| false
                    )
                    .is_ok(),
                "timestamp {timestamp} should be inside the window"
            );
        }

        for timestamp in [now - max - 1, now + max + 1] {
            let pkt = message(SENDER, timestamp);
            let rejection = guard
                .evaluate(
                    input(&pkt, &sender, None, &local, now),
                    &SelfBroadcastTracker::new(),
                    |_| false,
                )
                .expect_err("outside the window must be rejected");
            assert_eq!(
                rejection,
                IngressRejection::TimestampSkew {
                    peer: sender.clone(),
                    skew_millis: max + 1,
                    max_skew_millis: max,
                }
            );
        }
    }

    // --- self-broadcast --------------------------------------------------

    #[test]
    fn self_broadcast_tracker_resolves_then_forgets() {
        let pkt = message(LOCAL, 1234);
        let mut tracker = SelfBroadcastTracker::new();
        tracker.record("local-message", &pkt, 500);

        assert!(tracker.contains(&pkt));
        assert_eq!(tracker.message_id(&pkt), Some("local-message"));
        assert_eq!(tracker.take_message_id(&pkt).as_deref(), Some("local-message"));
        assert_eq!(tracker.take_message_id(&pkt), None);
        assert!(tracker.is_empty());
    }

    #[test]
    fn self_broadcast_key_ignores_ttl_but_not_sender_timestamp_or_type() {
        let original = message(LOCAL, 1234);
        let mut tracker = SelfBroadcastTracker::new();
        tracker.record("local-message", &original, 500);

        // A relay decrements the TTL; the echo must still match.
        let relayed = BitchatPacket {
            ttl: 1,
            ..original.clone()
        };
        assert!(tracker.contains(&relayed));

        // Any of the three key fields differing means a different packet.
        assert!(!tracker.contains(&message(LOCAL, 1235)));
        assert!(!tracker.contains(&message(SENDER, 1234)));
        assert!(!tracker.contains(&packet(MessageType::Leave, LOCAL, 1234, 3)));
    }

    #[test]
    fn self_broadcast_prune_keeps_entries_on_the_cutoff() {
        let old = message(LOCAL, 1);
        let fresh = message(LOCAL, 2);
        let mut tracker = SelfBroadcastTracker::new();
        tracker.record("old", &old, 90_000);
        tracker.record("fresh", &fresh, 100_000);

        tracker.prune(95_000);

        assert_eq!(tracker.take_message_id(&old), None);
        assert_eq!(tracker.take_message_id(&fresh).as_deref(), Some("fresh"));

        // Retention is `>=`: an entry exactly on the cutoff survives.
        let mut edge = SelfBroadcastTracker::new();
        edge.record("edge", &fresh, 95_000);
        edge.prune(95_000);
        assert_eq!(edge.len(), 1);

        edge.clear();
        assert!(edge.is_empty());
    }

    #[test]
    fn our_own_broadcast_echoed_back_by_a_relay_is_suppressed() {
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let sender = peer(SENDER);
        let now = 10_000_000;

        // The relay rewrote the sender ID as well as the TTL — a case the
        // sender-ID loopback check alone would miss, which is why the tracker keys
        // on the packet rather than on who claims to have sent it.
        let ours = message(LOCAL, now);
        let mut tracker = SelfBroadcastTracker::new();
        tracker.record("msg-1", &ours, now);

        let echo = BitchatPacket { ttl: 2, ..ours };
        let rejection = guard
            .evaluate(
                input(&echo, &sender, None, &local, now),
                &tracker,
                |_| false,
            )
            .expect_err("our own packet coming back must not be processed");
        assert_eq!(
            rejection,
            IngressRejection::SelfBroadcastEcho {
                msg_type: MessageType::Message.as_u8(),
                message_id: Some("msg-1".to_string()),
            }
        );
    }

    #[test]
    fn a_packet_claiming_our_sender_id_is_dropped_as_loopback() {
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let bound = peer(BOUND);
        let pkt = message(LOCAL, 1_000_000);

        let rejection = guard
            .evaluate(
                input(&pkt, &local, Some(&bound), &local, 1_000_000),
                &SelfBroadcastTracker::new(),
                |_| false,
            )
            .expect_err("a packet claiming to be from us is never processed");
        assert_eq!(
            rejection,
            IngressRejection::SelfLoopback {
                msg_type: MessageType::Message.as_u8(),
            }
        );
    }

    #[test]
    fn our_own_ttl_zero_sync_response_is_the_one_self_authored_exception() {
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let mut pkt = message(LOCAL, 0);
        pkt.ttl = 0;
        pkt.is_rsr = true;

        // Far outside the freshness window, but RSR-exempt and solicited.
        let accepted = guard
            .evaluate(
                input(&pkt, &local, None, &local, 10_000_000),
                &SelfBroadcastTracker::new(),
                |_| true,
            )
            .expect("a self-authored link-local sync response is not loopback");
        assert_eq!(accepted.validation_peer, local);
        assert!(accepted.freshness_exempt);

        // The exemption is narrow: TTL 1 makes it relayable, so it is loopback again.
        let relayable = BitchatPacket { ttl: 1, ..pkt };
        assert_eq!(
            guard
                .evaluate(
                    input(&relayable, &local, None, &local, 10_000_000),
                    &SelfBroadcastTracker::new(),
                    |_| true
                )
                .expect_err("a relayable packet from us is loopback"),
            IngressRejection::SelfLoopback {
                msg_type: MessageType::Message.as_u8()
            }
        );
    }

    // --- RSR solicitation ------------------------------------------------

    #[test]
    fn a_solicited_sync_response_is_exempt_from_freshness() {
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let bound = peer(BOUND);
        let sender = peer(SENDER);
        let mut pkt = message(SENDER, 1);
        pkt.is_rsr = true;

        let accepted = guard
            .evaluate(
                input(&pkt, &sender, Some(&bound), &local, 1_000_000),
                &SelfBroadcastTracker::new(),
                |p| *p == bound,
            )
            .expect("a response to a request we sent is exempt");

        // Validated against the link's peer, not the ID stamped on the packet.
        assert_eq!(accepted.received_from, bound);
        assert_eq!(accepted.validation_peer, bound);
        assert!(accepted.freshness_exempt);
    }

    #[test]
    fn an_unsolicited_sync_response_is_rejected_rather_than_exempted() {
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let bound = peer(BOUND);
        let sender = peer(SENDER);
        // Ancient timestamp: without the exemption gate this would be a free replay.
        let mut pkt = message(SENDER, 1);
        pkt.is_rsr = true;

        let rejection = guard
            .evaluate(
                input(&pkt, &sender, Some(&bound), &local, 1_000_000),
                &SelfBroadcastTracker::new(),
                |_| false,
            )
            .expect_err("the RSR flag alone must not buy an exemption");
        assert_eq!(
            rejection,
            IngressRejection::UnsolicitedSyncResponse { peer: bound }
        );
    }

    #[test]
    fn validate_payload_is_usable_on_its_own() {
        let guard = IngressGuard::default();
        let sender = peer(SENDER);
        let fresh = message(SENDER, 1_000_000);
        let ancient = message(SENDER, 1);

        assert!(
            guard
                .validate_payload(&fresh, &sender, 1_000_000, |_| false)
                .is_ok()
        );
        assert_eq!(
            guard
                .validate_payload(&ancient, &sender, 1_000_000, |_| false)
                .expect_err("ancient"),
            IngressRejection::TimestampSkew {
                peer: sender.clone(),
                skew_millis: 999_999,
                max_skew_millis: DEFAULT_MAX_TIMESTAMP_SKEW_MILLIS,
            }
        );
    }

    // --- link binding ----------------------------------------------------

    #[test]
    fn a_valid_packet_reports_both_the_link_peer_and_the_claimed_peer() {
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let bound = peer(BOUND);
        let sender = peer(SENDER);
        let pkt = message(SENDER, 1_000);

        let accepted = guard
            .evaluate(
                input(&pkt, &sender, Some(&bound), &local, 1_000),
                &SelfBroadcastTracker::new(),
                |_| false,
            )
            .expect("a fresh relayed packet on a bound link is fine");
        assert_eq!(accepted.received_from, bound);
        assert_eq!(accepted.validation_peer, sender);
        assert!(!accepted.direct_announce_hint);
        assert!(!accepted.freshness_exempt);
    }

    #[test]
    fn request_sync_claiming_a_different_sender_on_a_bound_link_is_spoofing() {
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let bound = peer(BOUND);
        let claimed = peer(SENDER);
        let mut pkt = packet(MessageType::RequestSync, SENDER, 0, 0);
        pkt.timestamp = 0;

        let rejection = guard
            .evaluate(
                input(&pkt, &claimed, Some(&bound), &local, 1_000_000),
                &SelfBroadcastTracker::new(),
                |_| false,
            )
            .expect_err("request-sync must come from the peer the link is bound to");
        // Binding is decided before freshness, so the reason is the spoof, not the
        // (also invalid) timestamp.
        assert_eq!(
            rejection,
            IngressRejection::DirectSenderMismatch {
                bound,
                claimed: claimed.clone(),
            }
        );
    }

    #[test]
    fn a_direct_announce_may_claim_a_new_sender_but_stays_subject_to_freshness() {
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let bound = peer(BOUND);
        let claimed = peer(SENDER);
        let now = 1_000_000;

        let fresh = packet(MessageType::Announce, SENDER, now, MESSAGE_TTL_DEFAULT);
        let accepted = guard
            .evaluate(
                input(&fresh, &claimed, Some(&bound), &local, now),
                &SelfBroadcastTracker::new(),
                |_| false,
            )
            .expect("a rotation heal is attributed to the claimed sender");
        assert_eq!(accepted.received_from, claimed);
        assert_eq!(accepted.validation_peer, claimed);
        // Forgeable: the TTL is outside the signature. Hint only.
        assert!(accepted.direct_announce_hint);

        let stale = packet(MessageType::Announce, SENDER, 0, MESSAGE_TTL_DEFAULT);
        let rejection = guard
            .evaluate(
                input(&stale, &claimed, Some(&bound), &local, now),
                &SelfBroadcastTracker::new(),
                |_| false,
            )
            .expect_err("the rotation path is not a freshness bypass");
        assert_eq!(
            rejection,
            IngressRejection::TimestampSkew {
                peer: claimed,
                skew_millis: now,
                max_skew_millis: DEFAULT_MAX_TIMESTAMP_SKEW_MILLIS,
            }
        );
    }

    #[test]
    fn a_relayed_announce_on_a_bound_link_is_not_a_rotation_candidate() {
        // TTL below the origination value: this announce came through a relay, so
        // the "rotation heal" shortcut does not apply and it is validated normally.
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let bound = peer(BOUND);
        let claimed = peer(SENDER);
        let now = 1_000_000;
        let relayed = packet(MessageType::Announce, SENDER, now, MESSAGE_TTL_DEFAULT - 1);

        let accepted = guard
            .evaluate(
                input(&relayed, &claimed, Some(&bound), &local, now),
                &SelfBroadcastTracker::new(),
                |_| false,
            )
            .expect("a relayed announce is ordinary traffic");
        assert_eq!(accepted.received_from, bound);
        assert_eq!(accepted.validation_peer, claimed);
        assert!(!accepted.direct_announce_hint);
    }

    #[test]
    fn an_unbound_link_falls_back_to_the_claimed_sender() {
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let sender = peer(SENDER);
        let pkt = message(SENDER, 1_000);

        let accepted = guard
            .evaluate(
                input(&pkt, &sender, None, &local, 1_000),
                &SelfBroadcastTracker::new(),
                |_| false,
            )
            .expect("nothing is bound yet");
        assert_eq!(accepted.received_from, sender);
        assert_eq!(accepted.validation_peer, sender);
    }

    #[test]
    fn a_signed_packet_is_not_treated_differently_by_the_guard() {
        // The guard runs before any signature check; carrying one changes nothing.
        let guard = IngressGuard::default();
        let local = peer(LOCAL);
        let sender = peer(SENDER);
        let pkt = BitchatPacket {
            signature: Some([0x5A; SIGNATURE_LEN]),
            ..message(SENDER, 1_000)
        };

        assert!(
            guard
                .evaluate(
                    input(&pkt, &sender, None, &local, 1_000),
                    &SelfBroadcastTracker::new(),
                    |_| false
                )
                .is_ok()
        );
    }

    #[test]
    fn a_custom_guard_configuration_is_honoured() {
        let guard = IngressGuard::new(4, 1_000);
        let announce = packet(MessageType::Announce, SENDER, 1_000, 4);
        assert!(guard.is_direct_announce_hint(&announce));
        assert!(!IngressGuard::default().is_direct_announce_hint(&announce));

        let local = peer(LOCAL);
        let sender = peer(SENDER);
        let pkt = message(SENDER, 0);
        let rejection = guard
            .evaluate(
                input(&pkt, &sender, None, &local, 1_001),
                &SelfBroadcastTracker::new(),
                |_| false,
            )
            .expect_err("1001ms of skew exceeds the 1000ms limit");
        assert_eq!(
            rejection,
            IngressRejection::TimestampSkew {
                peer: sender,
                skew_millis: 1_001,
                max_skew_millis: 1_000,
            }
        );
    }
}
