//! Courier delivery over the internet bridge.
//!
//! Ported from `Services/Gateway/BridgeCourierService.swift`.
//!
//! A courier envelope normally waits for a human being to walk from the sender to the
//! recipient. A *drop* is the same sealed envelope parked on public Nostr relays as a
//! kind-1401 event, tagged with the recipient's rotating tag — so delivery stops
//! requiring a physical encounter while nothing about the envelope changes.
//!
//! # Two duties, both gated on the bridge toggle
//!
//! - **Sender.** When the router seals mail for an unreachable peer it deposits a copy
//!   here; we publish it as a drop, or queue it (bounded, drop-oldest) until relays
//!   connect.
//! - **Recipient.** We subscribe for our own candidate tags across adjacent UTC days
//!   and open matching drops. The adjacency is not decoration: the tag rotates at UTC
//!   midnight, so a drop published a minute before rollover is only findable if the
//!   recipient is still watching yesterday's tag.
//!
//! bitchat also has a gateway role (watching verified local peers' tags and
//! republishing carried mail). This device never runs as a gateway, so it is not
//! ported.
//!
//! # The publisher key is fresh every single time
//!
//! [`DropRelay::fresh_publisher_key`] is called once per publish attempt and the key
//! is moved into that attempt's [`PublishRequest`]; nothing here ever stores one.
//!
//! The envelope authenticates its sender internally via the Noise-X seal, so a stable
//! publisher key adds no authenticity — it only gives relays a handle with which to
//! link every courier drop one user has ever sent into a single traffic pattern. This
//! is the property the whole design rests on. A refactor that hoists the key out of
//! the attempt loop "because generating one each time is wasteful" silently destroys
//! it, which is why the key rides in the request rather than living on the seam.
//!
//! # What a drop reveals
//!
//! To a relay: that *someone* published an opaque blob addressed to *some* 16-byte
//! day-rotating tag. Only a party that already knows the recipient's Noise static key
//! can compute that tag, and the payload is an opaque seal. Nothing here may add a
//! field that narrows either of those.
//!
//! Duplicate deliveries — a drop, a physical courier and a direct link all landing the
//! same message — are absorbed downstream by message-ID dedup. The redundancy is free
//! and deliberate; it must not be "optimised" away.
//!
//! # Sans-IO
//!
//! No clock, no timers, no sockets. Time arrives as `now_unix_secs`, relay results
//! arrive through [`BridgeCourier::resolve_publish`], and the periodic refresh the
//! Swift arms with a `Task` is expressed as [`BridgeCourier::next_refresh_due`] for
//! whoever owns the runtime to wake on.

use std::collections::VecDeque;
use std::fmt;

use chatter_mesh::tlv::courier_envelope::{CourierEnvelope, TAG_LEN};

/// Drops waiting for relay connectivity. Bounded, drop-oldest.
pub const MAX_PENDING_DROPS: usize = 20;
/// Tag-set refresh cadence. Also what covers the UTC day rollover.
pub const REFRESH_INTERVAL_SECS: u64 = 30 * 60;
/// Encoded-envelope cap for a drop: 16 KiB of ciphertext plus TLV slack.
pub const MAX_DROP_ENVELOPE_BYTES: usize = 20 * 1024;
/// Capacity of each of the dedup sets.
pub const MAX_TRACKED_IDS: usize = 512;
/// How long a dedup entry lives — the courier envelope lifetime, which is also the
/// NIP-40 window relays hold a drop for.
pub const TRACKED_ID_LIFETIME_SECS: u64 = crate::MESSAGE_TTL_SECS;

/// Identifies one publish attempt chain.
///
/// Minted per deposit and carried through queueing and
/// retries, so a late relay callback for an operation that was evicted, cancelled or
/// superseded can be recognised and ignored rather than completing a newer attempt for
/// the same message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DropOperationId(u64);

impl DropOperationId {
    /// The raw counter, for a caller that needs to key its own map by it.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DropOperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "drop#{}", self.0)
    }
}

/// A single-use secret for signing exactly one drop event.
///
/// 32 bytes, which is what a Nostr identity is built from. It is deliberately not
/// `Clone`: a key that can be copied is a key that can be reused, and reuse is the one
/// thing this type exists to prevent.
#[derive(PartialEq, Eq)]
pub struct PublisherKey([u8; 32]);

impl PublisherKey {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Consumes the key. The publisher signs once and the bytes go away with it.
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for PublisherKey {
    /// Redacted: a secret that prints itself ends up in a log eventually.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PublisherKey(<redacted>)")
    }
}

/// Everything the relay side needs to compose one kind-1401 event.
///
/// Maps one-to-one onto `chatter_mesh_nostr::builders::courier_drop`. Not `Clone` —
/// republishing means a new attempt with a new key, never a second copy of this.
#[derive(Debug)]
pub struct PublishRequest<'a> {
    /// Echo this back to [`BridgeCourier::resolve_publish`] with the NIP-20 result.
    pub operation: DropOperationId,
    /// The rotating tag the event is filed under (`x` tag, hex-encoded by the caller).
    pub recipient_tag: [u8; TAG_LEN],
    /// The encoded [`CourierEnvelope`]. Opaque; base64 it into the event content.
    pub sealed_envelope: &'a [u8],
    /// NIP-40 expiration, unix seconds, strictly in the future.
    pub expires_at_unix_secs: u64,
    /// Fresh for this attempt and never reused. See the module docs.
    pub publisher_key: PublisherKey,
}

/// The relay seam.
///
/// This crate does not depend on `chatter-mesh-nostr`: that coupling belongs to the
/// integration layer, and keeping it out means the whole policy here — queueing,
/// cooldowns, tag rotation, dedup — is testable without a relay or a network.
pub trait DropRelay {
    /// Whether at least one relay connection is live. A publish attempt while this is
    /// false queues instead.
    fn is_connected(&self) -> bool;

    /// Mint a throwaway signing key.
    ///
    /// **Implementations must return fresh random bytes on every call.** Returning a
    /// stored key would let relays link every drop this device has ever published;
    /// see the module docs.
    fn fresh_publisher_key(&self) -> PublisherKey;

    /// Hand a drop to the relays. The result is asynchronous — a NIP-20 `OK` from at
    /// least one relay — and arrives back through [`BridgeCourier::resolve_publish`].
    /// "Written to a socket" and "queued in RAM" are not acceptance.
    fn publish(&self, request: PublishRequest<'_>);

    /// (Re)open the drop subscription for exactly these tags. Sorted and deduplicated.
    fn open_subscription(&self, tags: &[[u8; TAG_LEN]]);

    fn close_subscription(&self);
}

/// Where an inbound drop goes once it is ours to act on.
///
/// Synchronous, unlike publishing: the decision is local. A `false` means the drop
/// was *not* consumed and the relay event stays eligible for another attempt.
pub trait DropSink {
    /// Open a drop addressed to us.
    ///
    /// `true` means the inner envelope was delivered, deduplicated, or intentionally
    /// rejected after decryption — all terminal. `false` is a transient crypto or key
    /// failure and leaves the event retryable.
    fn open_for_self(&self, envelope: &CourierEnvelope) -> bool;
}

/// Why a deposit was refused. None of these are failures — they are the bounds doing
/// their job — but they mean different things to a caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepositRefusal {
    /// The bridge toggle is off. Nothing is published while it is.
    BridgeDisabled,
    /// A relay already accepted a drop for this message ID.
    AlreadyPublished,
    /// An attempt for this message ID is queued or awaiting a relay result.
    AlreadyInFlight,
    /// The envelope cannot be encoded, or exceeds [`MAX_DROP_ENVELOPE_BYTES`].
    /// Deterministic: the same content fails the same way every time, so the message
    /// ID is suppressed for this process rather than retried.
    Undeliverable,
    /// The envelope's own expiry has passed, or is not far enough in the future to
    /// carry a NIP-40 expiration.
    Expired,
}

/// What became of a deposit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepositOutcome {
    /// Handed to the relays. Watch for [`BridgeCourier::resolve_publish`].
    Publishing(DropOperationId),
    /// Relays are down; queued until the next flush.
    Queued(DropOperationId),
    Refused(DepositRefusal),
}

/// A deposit, plus anything the deposit displaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deposited {
    pub outcome: DepositOutcome,
    /// Queued drops evicted to make room, oldest first. These operations will never
    /// resolve; their mail is still in the outbox and may be deposited again.
    pub abandoned: Vec<AbandonedDrop>,
}

/// An operation that ended without a relay result: evicted from the pending queue, or
/// cancelled by a bridge disable or a panic wipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbandonedDrop {
    pub operation: DropOperationId,
    /// The sender-side message ID.
    pub message_id: String,
}

/// The result of a publish attempt, matched back to what it was publishing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedDrop {
    pub operation: DropOperationId,
    pub message_id: String,
    pub accepted: bool,
}

/// What happened to an inbound drop event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropDisposition {
    /// Handed to [`DropSink::open_for_self`], which took it.
    OpenedForSelf,
    /// The sink refused transiently. The event ID is *not* consumed, so the same drop
    /// arriving again — from another relay, or after a resubscribe — is tried again.
    Retryable,
    Ignored(IgnoreReason),
}

/// Why an inbound drop went no further.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoreReason {
    BridgeDisabled,
    /// This event ID was already handled, possibly in an earlier process.
    AlreadyHandled,
    /// Larger than [`MAX_DROP_ENVELOPE_BYTES`], or not a decodable envelope.
    Malformed,
    Expired,
    /// The envelope's own recipient tag disagrees with the event tag it was filed
    /// under. A mislabelled drop must not ride a subscription it does not belong to.
    Mislabelled,
    /// Addressed to a tag we are not watching — a leftover from a previous tag set,
    /// or someone else's mail entirely.
    NotWatched,
}

/// One drop event, as delivered by the subscription.
///
/// The relay layer has already verified the event signature, base64-decoded the
/// content and hex-decoded the `x` tag; what arrives here is bytes. Comparing tags as
/// bytes rather than hex strings also removes any question of case.
#[derive(Clone, Copy, Debug)]
pub struct DropEvent<'a> {
    /// The Nostr event ID, for multi-relay deduplication.
    pub event_id: &'a str,
    /// The event's filterable `x` tag.
    pub tag: [u8; TAG_LEN],
    /// The event content: an encoded [`CourierEnvelope`].
    pub envelope_bytes: &'a [u8],
}

/// The dedup record, for a caller that can persist it.
///
/// Persisting matters. Relays hold drops for the full 24 h NIP-40 window and the
/// outbox keeps re-depositing, so an in-memory-only record means every relaunch
/// republishes the same message as a fresh drop and reopens every drop already
/// handled. bitchat observed exactly that amplification storm in the field.
///
/// Entries are `(id, recorded_at_unix_secs)` and age out with the drop window.
/// Deterministically undeliverable message IDs are intentionally absent: they are
/// suppressed for the process, but recording them would look like a relay had accepted
/// them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DedupSnapshot {
    /// Message IDs a relay has confirmed a drop for.
    pub published_drop_keys: Vec<(String, u64)>,
    /// Drop event IDs already opened.
    pub seen_drop_event_ids: Vec<(String, u64)>,
}

// --- The service -----------------------------------------------------------

/// Bridge courier drops: publishing our own kind-1401 drops and opening ours.
pub struct BridgeCourier<R, S> {
    pub(super) relay: R,
    pub(super) sink: S,
    pub(super) bridge_enabled: bool,
    pub(super) my_tags: Vec<[u8; TAG_LEN]>,
    pub(super) pending: VecDeque<PendingDrop>,
    pub(super) in_flight: Vec<InFlightDrop>,
    pub(super) published_drop_keys: ExpiringIdSet,
    pub(super) seen_drop_event_ids: ExpiringIdSet,
    pub(super) rejected_drop_keys: ExpiringIdSet,
    pub(super) subscription_open: bool,
    pub(super) last_subscribed: Vec<[u8; TAG_LEN]>,
    pub(super) last_refresh: u64,
    pub(super) next_operation: u64,
}

#[derive(Clone, Debug)]
pub(super) struct PendingDrop {
    pub(super) operation: DropOperationId,
    pub(super) message_id: String,
    pub(super) envelope: CourierEnvelope,
}

#[derive(Clone, Debug)]
pub(super) struct InFlightDrop {
    pub(super) operation: DropOperationId,
    pub(super) message_id: String,
}

/// Where one publish attempt ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AttemptState {
    Published,
    Queued,
    Dropped,
}

pub(super) struct AttemptResult {
    pub(super) state: AttemptState,
    pub(super) abandoned: Vec<AbandonedDrop>,
}

impl<R: DropRelay, S: DropSink> BridgeCourier<R, S> {
    pub fn new(relay: R, sink: S) -> Self {
        Self {
            relay,
            sink,
            bridge_enabled: false,
            my_tags: Vec::new(),
            pending: VecDeque::new(),
            in_flight: Vec::new(),
            published_drop_keys: ExpiringIdSet::new(MAX_TRACKED_IDS, TRACKED_ID_LIFETIME_SECS),
            seen_drop_event_ids: ExpiringIdSet::new(MAX_TRACKED_IDS, TRACKED_ID_LIFETIME_SECS),
            rejected_drop_keys: ExpiringIdSet::new(MAX_TRACKED_IDS, TRACKED_ID_LIFETIME_SECS),
            subscription_open: false,
            last_subscribed: Vec::new(),
            last_refresh: 0,
            next_operation: 1,
        }
    }

    pub fn relay(&self) -> &R {
        &self.relay
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// The bridge toggle. Call [`refresh`](Self::refresh) afterwards: turning it off
    /// is what cancels queued and in-flight drops and closes the subscription.
    pub fn set_bridge_enabled(&mut self, enabled: bool) {
        self.bridge_enabled = enabled;
    }

    pub fn is_bridge_enabled(&self) -> bool {
        self.bridge_enabled
    }

    /// Our own watched tags: the adjacent-day candidates for our static key.
    pub fn my_tags(&self) -> &[[u8; TAG_LEN]] {
        &self.my_tags
    }

    /// Drops waiting for relay connectivity.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Attempts awaiting a relay result.
    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    pub fn is_subscribed(&self) -> bool {
        self.subscription_open
    }
}

/// The NIP-40 expiration for a drop, or `None` when the envelope is spent.
///
/// The envelope's expiry is in milliseconds and the tag is in seconds, so an envelope
/// expiring later this same second truncates to "expires now" — which a relay would
/// reject and which is not worth publishing anyway. Requiring it to be strictly in the
/// future folds that case in with the already-expired one.
pub(super) fn expires_at_secs(envelope: &CourierEnvelope, now_unix_secs: u64) -> Option<u64> {
    let seconds = envelope.expiry_ms / 1_000;
    (seconds > now_unix_secs).then_some(seconds)
}

pub(super) fn millis(unix_secs: u64) -> u64 {
    unix_secs.saturating_mul(1_000)
}

mod dedup;
mod expiring_id_set;
mod sender;
mod subscription;

use expiring_id_set::ExpiringIdSet;

#[cfg(test)]
mod tests;
