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

use chatter_mesh::tlv::courier_envelope::{CourierEnvelope, TAG_LEN, candidate_tags};

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
    relay: R,
    sink: S,
    bridge_enabled: bool,
    my_tags: Vec<[u8; TAG_LEN]>,
    pending: VecDeque<PendingDrop>,
    in_flight: Vec<InFlightDrop>,
    published_drop_keys: ExpiringIdSet,
    seen_drop_event_ids: ExpiringIdSet,
    rejected_drop_keys: ExpiringIdSet,
    subscription_open: bool,
    last_subscribed: Vec<[u8; TAG_LEN]>,
    last_refresh: u64,
    next_operation: u64,
}

#[derive(Clone, Debug)]
struct PendingDrop {
    operation: DropOperationId,
    message_id: String,
    envelope: CourierEnvelope,
}

#[derive(Clone, Debug)]
struct InFlightDrop {
    operation: DropOperationId,
    message_id: String,
}

/// Where one publish attempt ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptState {
    Published,
    Queued,
    Dropped,
}

struct AttemptResult {
    state: AttemptState,
    abandoned: Vec<AbandonedDrop>,
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

    // --- Sender role -------------------------------------------------------

    /// Publish a sealed copy of outbound mail as a relay drop.
    ///
    /// Called by the router alongside its physical courier deposits — the two are
    /// parallel, not alternatives, and a message delivered twice is deduplicated
    /// downstream. Idempotent per message ID: a second deposit while the first is
    /// queued, in flight, or already accepted is refused rather than duplicated.
    pub fn deposit(
        &mut self,
        message_id: &str,
        envelope: &CourierEnvelope,
        now_unix_secs: u64,
    ) -> Deposited {
        let refuse = |reason| Deposited {
            outcome: DepositOutcome::Refused(reason),
            abandoned: Vec::new(),
        };

        if !self.bridge_enabled {
            return refuse(DepositRefusal::BridgeDisabled);
        }
        if self.published_drop_keys.contains(message_id, now_unix_secs) {
            return refuse(DepositRefusal::AlreadyPublished);
        }
        if self.rejected_drop_keys.contains(message_id, now_unix_secs) {
            return refuse(DepositRefusal::Undeliverable);
        }
        if self.is_tracked(message_id) {
            return refuse(DepositRefusal::AlreadyInFlight);
        }

        // An envelope that cannot encode within the drop caps fails identically on
        // every attempt — the size is a function of the content, not of the sealing.
        // Suppress it for this process so the retry sweep does not churn, but never
        // record it as published: no relay ever saw it.
        match envelope.encode() {
            Ok(encoded) if encoded.len() <= MAX_DROP_ENVELOPE_BYTES => {}
            _ => {
                self.rejected_drop_keys.insert(message_id, now_unix_secs);
                return refuse(DepositRefusal::Undeliverable);
            }
        }
        if expires_at_secs(envelope, now_unix_secs).is_none() {
            return refuse(DepositRefusal::Expired);
        }

        let operation = self.mint_operation();
        let attempt = self.attempt_publish(
            operation,
            message_id.to_owned(),
            envelope.clone(),
            now_unix_secs,
        );
        let outcome = match attempt.state {
            AttemptState::Published => DepositOutcome::Publishing(operation),
            AttemptState::Queued => DepositOutcome::Queued(operation),
            AttemptState::Dropped => DepositOutcome::Refused(DepositRefusal::Undeliverable),
        };
        Deposited {
            outcome,
            abandoned: attempt.abandoned,
        }
    }

    /// Report a relay's verdict on a publish attempt.
    ///
    /// `accepted` must mean a relay explicitly acknowledged the event (NIP-20 `OK`).
    /// Only that commits the sender-side dedup key.
    ///
    /// Returns `None` for an operation this instance is not waiting on: a duplicated
    /// callback, one invalidated by a wipe or a bridge disable, or one belonging to a
    /// superseded attempt. Completing a newer attempt from an older callback is
    /// exactly the confusion the operation ID exists to prevent.
    pub fn resolve_publish(
        &mut self,
        operation: DropOperationId,
        accepted: bool,
        now_unix_secs: u64,
    ) -> Option<ResolvedDrop> {
        let index = self
            .in_flight
            .iter()
            .position(|entry| entry.operation == operation)?;
        let entry = self.in_flight.remove(index);

        if accepted {
            self.published_drop_keys
                .insert(&entry.message_id, now_unix_secs);
        }

        Some(ResolvedDrop {
            operation,
            message_id: entry.message_id,
            accepted,
        })
    }

    /// Publish the drops that were queued while relays were unreachable.
    ///
    /// Each one is a fresh attempt: re-encoded, re-checked against the caps and the
    /// expiry, and signed with a new throwaway key.
    pub fn flush_pending(&mut self, now_unix_secs: u64) -> Vec<AbandonedDrop> {
        if !self.bridge_enabled || !self.relay.is_connected() || self.pending.is_empty() {
            return Vec::new();
        }
        let queued: Vec<PendingDrop> = self.pending.drain(..).collect();
        let mut abandoned = Vec::new();
        for item in queued {
            let attempt = self.attempt_publish(
                item.operation,
                item.message_id,
                item.envelope,
                now_unix_secs,
            );
            abandoned.extend(attempt.abandoned);
        }
        abandoned
    }

    // --- Subscription ------------------------------------------------------

    /// Recompute our tag set and (re)open the subscription, then drain the queue.
    ///
    /// Call on toggle changes, on relay connectivity changes, and whenever
    /// [`next_refresh_due`](Self::next_refresh_due) comes around — the periodic call is
    /// what carries the tag set across UTC midnight. Idempotent.
    ///
    /// `local_noise_key` is our own Noise static public key; `None` before an
    /// identity exists, in which case there is nothing to watch.
    pub fn refresh(
        &mut self,
        local_noise_key: Option<[u8; 32]>,
        now_unix_secs: u64,
    ) -> Vec<AbandonedDrop> {
        self.last_refresh = now_unix_secs;

        if !self.bridge_enabled {
            let abandoned = self.cancel_active_publishes();
            self.close_subscription_if_open();
            return abandoned;
        }
        if !self.relay.is_connected() {
            // Keep the tag sets: they are still what an event delivered by a
            // subscription that is only now tearing down should be matched against.
            self.close_subscription_if_open();
            return Vec::new();
        }

        self.my_tags = match local_noise_key {
            Some(key) => candidate_tags(&key, now_unix_secs).to_vec(),
            None => Vec::new(),
        };

        let mut all_tags: Vec<[u8; TAG_LEN]> = self.my_tags.clone();
        all_tags.sort_unstable();
        all_tags.dedup();

        if all_tags.is_empty() {
            self.close_subscription_if_open();
            self.last_subscribed.clear();
            return Vec::new();
        }

        // Resubscribe only when the set actually changed: bitchat's field logs
        // showed the drop subscription being rebuilt every minute or so for an
        // unchanged tag set.
        if !self.subscription_open || all_tags != self.last_subscribed {
            self.relay.open_subscription(&all_tags);
            self.subscription_open = true;
            self.last_subscribed = all_tags;
        }

        self.flush_pending(now_unix_secs)
    }

    /// When [`refresh`](Self::refresh) should next be called, in unix seconds.
    ///
    /// The sans-IO form of the Swift's half-hourly tag-refresh timer. A caller that
    /// only ever refreshes on connectivity changes still works; it just discovers the
    /// day rollover late.
    pub fn next_refresh_due(&self) -> u64 {
        self.last_refresh.saturating_add(REFRESH_INTERVAL_SECS)
    }

    // --- Inbound drops -----------------------------------------------------

    /// Handle one drop event delivered by the subscription.
    ///
    /// The event's signature has already been verified by the relay layer; everything
    /// this checks is about whether the drop is ours to act on.
    pub fn handle_drop(&mut self, event: &DropEvent<'_>, now_unix_secs: u64) -> DropDisposition {
        if !self.bridge_enabled {
            return DropDisposition::Ignored(IgnoreReason::BridgeDisabled);
        }
        // A resubscribe can still deliver an event from the old tag set, so the
        // event ID is not consumed until the drop is actually opened.
        if self
            .seen_drop_event_ids
            .contains(event.event_id, now_unix_secs)
        {
            return DropDisposition::Ignored(IgnoreReason::AlreadyHandled);
        }
        if event.envelope_bytes.len() > MAX_DROP_ENVELOPE_BYTES {
            return DropDisposition::Ignored(IgnoreReason::Malformed);
        }
        let Ok(envelope) = CourierEnvelope::decode(event.envelope_bytes) else {
            return DropDisposition::Ignored(IgnoreReason::Malformed);
        };
        if envelope.is_expired(millis(now_unix_secs)) {
            return DropDisposition::Ignored(IgnoreReason::Expired);
        }
        // The envelope's own tag must agree with the tag the event was filed under, or
        // a mislabelled drop could ride a subscription it does not belong to.
        if envelope.recipient_tag != event.tag {
            return DropDisposition::Ignored(IgnoreReason::Mislabelled);
        }

        if !self.my_tags.contains(&event.tag) {
            return DropDisposition::Ignored(IgnoreReason::NotWatched);
        }
        if self.sink.open_for_self(&envelope) {
            self.seen_drop_event_ids
                .insert(event.event_id, now_unix_secs);
            DropDisposition::OpenedForSelf
        } else {
            DropDisposition::Retryable
        }
    }

    // --- Dedup record ------------------------------------------------------

    /// The dedup record, for persisting across relaunches. See [`DedupSnapshot`].
    pub fn dedup_snapshot(&self) -> DedupSnapshot {
        DedupSnapshot {
            published_drop_keys: self.published_drop_keys.entries(),
            seen_drop_event_ids: self.seen_drop_event_ids.entries(),
        }
    }

    /// Reinstate a persisted dedup record. Entries already past the drop window are
    /// discarded rather than restored.
    pub fn restore_dedup(&mut self, snapshot: &DedupSnapshot, now_unix_secs: u64) {
        self.published_drop_keys
            .restore(&snapshot.published_drop_keys, now_unix_secs);
        self.seen_drop_event_ids
            .restore(&snapshot.seen_drop_event_ids, now_unix_secs);
    }

    /// Panic wipe: forget every queued drop, every in-flight attempt, and the whole
    /// dedup record. The caller must wipe its persisted copy too.
    pub fn wipe(&mut self) -> Vec<AbandonedDrop> {
        let abandoned = self.cancel_active_publishes();
        self.published_drop_keys.clear();
        self.seen_drop_event_ids.clear();
        self.rejected_drop_keys.clear();
        self.my_tags.clear();
        self.last_subscribed.clear();
        abandoned
    }

    // --- Internals ---------------------------------------------------------

    fn mint_operation(&mut self) -> DropOperationId {
        let id = DropOperationId(self.next_operation);
        self.next_operation = self.next_operation.wrapping_add(1);
        id
    }

    /// One publish attempt: publish now, queue for later, or give up.
    ///
    /// The throwaway key is requested here and moved straight into the request, so
    /// every attempt — including every retry of the same envelope — signs under a key
    /// nobody has seen before.
    fn attempt_publish(
        &mut self,
        operation: DropOperationId,
        message_id: String,
        envelope: CourierEnvelope,
        now_unix_secs: u64,
    ) -> AttemptResult {
        let dropped = AttemptResult {
            state: AttemptState::Dropped,
            abandoned: Vec::new(),
        };

        let Ok(encoded) = envelope.encode() else {
            return dropped;
        };
        if encoded.len() > MAX_DROP_ENVELOPE_BYTES {
            return dropped;
        }
        let Some(expires_at_unix_secs) = expires_at_secs(&envelope, now_unix_secs) else {
            return dropped;
        };

        if !self.relay.is_connected() {
            return AttemptResult {
                state: AttemptState::Queued,
                abandoned: self.enqueue(operation, message_id, envelope),
            };
        }

        self.in_flight.push(InFlightDrop {
            operation,
            message_id,
        });
        self.relay.publish(PublishRequest {
            operation,
            recipient_tag: envelope.recipient_tag,
            sealed_envelope: &encoded,
            expires_at_unix_secs,
            publisher_key: self.relay.fresh_publisher_key(),
        });
        AttemptResult {
            state: AttemptState::Published,
            abandoned: Vec::new(),
        }
    }

    /// Queue a drop, evicting the oldest once the bound is exceeded.
    fn enqueue(
        &mut self,
        operation: DropOperationId,
        message_id: String,
        envelope: CourierEnvelope,
    ) -> Vec<AbandonedDrop> {
        self.pending.push_back(PendingDrop {
            operation,
            message_id,
            envelope,
        });
        let mut abandoned = Vec::new();
        while self.pending.len() > MAX_PENDING_DROPS {
            if let Some(evicted) = self.pending.pop_front() {
                abandoned.push(AbandonedDrop {
                    operation: evicted.operation,
                    message_id: evicted.message_id,
                });
            }
        }
        abandoned
    }

    /// Whether a message ID already has an attempt queued or awaiting a relay result.
    fn is_tracked(&self, message_id: &str) -> bool {
        let queued = self
            .pending
            .iter()
            .any(|item| item.message_id == message_id);
        let in_flight = self
            .in_flight
            .iter()
            .any(|entry| entry.message_id == message_id);
        queued || in_flight
    }

    fn close_subscription_if_open(&mut self) {
        if self.subscription_open {
            self.relay.close_subscription();
            self.subscription_open = false;
        }
    }

    /// Invalidate everything queued or awaiting a relay result. Late callbacks for
    /// these operations find no entry and no-op.
    fn cancel_active_publishes(&mut self) -> Vec<AbandonedDrop> {
        let mut abandoned: Vec<AbandonedDrop> = self
            .pending
            .drain(..)
            .map(|item| AbandonedDrop {
                operation: item.operation,
                message_id: item.message_id,
            })
            .collect();
        abandoned.extend(self.in_flight.drain(..).map(|entry| AbandonedDrop {
            operation: entry.operation,
            message_id: entry.message_id,
        }));
        abandoned
    }
}

/// The NIP-40 expiration for a drop, or `None` when the envelope is spent.
///
/// The envelope's expiry is in milliseconds and the tag is in seconds, so an envelope
/// expiring later this same second truncates to "expires now" — which a relay would
/// reject and which is not worth publishing anyway. Requiring it to be strictly in the
/// future folds that case in with the already-expired one.
fn expires_at_secs(envelope: &CourierEnvelope, now_unix_secs: u64) -> Option<u64> {
    let seconds = envelope.expiry_ms / 1_000;
    (seconds > now_unix_secs).then_some(seconds)
}

fn millis(unix_secs: u64) -> u64 {
    unix_secs.saturating_mul(1_000)
}

// --- Expiring ID set -------------------------------------------------------

/// A bounded, ageing set of IDs.
///
/// [`chatter_mesh::dedup::MessageDeduplicator`] is the same idea, but it cannot hand
/// its entries back out and its `contains` ignores the age — and both matter here,
/// because this record is persisted across relaunches and queried without recording.
struct ExpiringIdSet {
    entries: VecDeque<(String, u64)>,
    capacity: usize,
    lifetime_secs: u64,
}

impl ExpiringIdSet {
    fn new(capacity: usize, lifetime_secs: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
            lifetime_secs,
        }
    }

    fn is_live(&self, recorded_at: u64, now: u64) -> bool {
        now.saturating_sub(recorded_at) < self.lifetime_secs
    }

    /// Present and not yet aged out. Does not record anything.
    fn contains(&self, id: &str, now: u64) -> bool {
        self.entries
            .iter()
            .any(|(entry, at)| entry == id && self.is_live(*at, now))
    }

    fn insert(&mut self, id: &str, now: u64) {
        self.prune(now);
        if let Some(entry) = self.entries.iter_mut().find(|(entry, _)| entry == id) {
            entry.1 = now;
            return;
        }
        self.entries.push_back((id.to_owned(), now));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    fn prune(&mut self, now: u64) {
        let lifetime = self.lifetime_secs;
        self.entries
            .retain(|(_, at)| now.saturating_sub(*at) < lifetime);
    }

    fn entries(&self) -> Vec<(String, u64)> {
        self.entries.iter().cloned().collect()
    }

    fn restore(&mut self, entries: &[(String, u64)], now: u64) {
        for (id, at) in entries {
            if self.is_live(*at, now) {
                self.insert(id, *at);
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const DAY: u64 = 86_400;
    const NOW: u64 = 1_700_000_000;

    // --- Fakes -------------------------------------------------------------

    #[derive(Debug)]
    struct SentDrop {
        operation: DropOperationId,
        tag: [u8; TAG_LEN],
        envelope: Vec<u8>,
        expires_at: u64,
        publisher_key: [u8; 32],
    }

    #[derive(Default)]
    struct FakeRelay {
        connected: RefCell<bool>,
        keys_minted: RefCell<u64>,
        published: RefCell<Vec<SentDrop>>,
        subscriptions: RefCell<Vec<Vec<[u8; TAG_LEN]>>>,
        closes: RefCell<usize>,
    }

    impl FakeRelay {
        fn set_connected(&self, connected: bool) {
            *self.connected.borrow_mut() = connected;
        }

        fn publish_count(&self) -> usize {
            self.published.borrow().len()
        }

        fn last_operation(&self) -> DropOperationId {
            self.published.borrow().last().expect("a publish").operation
        }

        fn publisher_keys(&self) -> Vec<[u8; 32]> {
            self.published
                .borrow()
                .iter()
                .map(|drop| drop.publisher_key)
                .collect()
        }
    }

    impl DropRelay for FakeRelay {
        fn is_connected(&self) -> bool {
            *self.connected.borrow()
        }

        fn fresh_publisher_key(&self) -> PublisherKey {
            let mut minted = self.keys_minted.borrow_mut();
            *minted += 1;
            // Distinct per call, which is all the module can be asked to prove: a real
            // implementation draws from the CSPRNG.
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&minted.to_be_bytes());
            PublisherKey::from_bytes(bytes)
        }

        fn publish(&self, request: PublishRequest<'_>) {
            self.published.borrow_mut().push(SentDrop {
                operation: request.operation,
                tag: request.recipient_tag,
                envelope: request.sealed_envelope.to_vec(),
                expires_at: request.expires_at_unix_secs,
                publisher_key: *request.publisher_key.as_bytes(),
            });
        }

        fn open_subscription(&self, tags: &[[u8; TAG_LEN]]) {
            self.subscriptions.borrow_mut().push(tags.to_vec());
        }

        fn close_subscription(&self) {
            *self.closes.borrow_mut() += 1;
        }
    }

    #[derive(Default)]
    struct FakeSink {
        opened: RefCell<Vec<CourierEnvelope>>,
        accept_open: RefCell<bool>,
    }

    impl FakeSink {
        fn accepting() -> Self {
            let sink = Self::default();
            *sink.accept_open.borrow_mut() = true;
            sink
        }
    }

    impl DropSink for FakeSink {
        fn open_for_self(&self, envelope: &CourierEnvelope) -> bool {
            self.opened.borrow_mut().push(envelope.clone());
            *self.accept_open.borrow()
        }
    }

    // --- Helpers -----------------------------------------------------------

    fn new_courier(connected: bool) -> BridgeCourier<FakeRelay, FakeSink> {
        let relay = FakeRelay::default();
        relay.set_connected(connected);
        let mut courier = BridgeCourier::new(relay, FakeSink::accepting());
        courier.set_bridge_enabled(true);
        courier
    }

    fn envelope_for(key: &[u8; 32], now: u64, body: u8) -> CourierEnvelope {
        CourierEnvelope::new(
            chatter_mesh::tlv::courier_envelope::recipient_tag(
                key,
                chatter_mesh::tlv::courier_envelope::epoch_day(now),
            ),
            millis(now + DAY),
            vec![body; 32],
            1,
            None,
        )
    }

    // --- The non-negotiable ------------------------------------------------

    #[test]
    fn every_publish_uses_a_fresh_throwaway_key() {
        let mut courier = new_courier(true);
        let key = [0x11u8; 32];
        let envelope = envelope_for(&key, NOW, 1);

        let first = courier.deposit("m1", &envelope, NOW);
        assert!(matches!(first.outcome, DepositOutcome::Publishing(_)));
        // The same envelope, deposited again under a different message ID: byte for
        // byte identical mail, and still a key nobody has seen.
        let second = courier.deposit("m2", &envelope, NOW);
        assert!(matches!(second.outcome, DepositOutcome::Publishing(_)));

        let keys = courier.relay().publisher_keys();
        assert_eq!(keys.len(), 2);
        assert_ne!(
            keys[0], keys[1],
            "a reused publisher key lets relays link every drop a user sends"
        );
        assert_eq!(
            *courier.relay().keys_minted.borrow(),
            2,
            "one key per publish"
        );
    }

    #[test]
    fn a_retry_of_the_same_drop_also_gets_a_new_key() {
        let mut courier = new_courier(false);
        let key = [0x12u8; 32];
        let envelope = envelope_for(&key, NOW, 2);

        let deposited = courier.deposit("m1", &envelope, NOW);
        assert!(matches!(deposited.outcome, DepositOutcome::Queued(_)));
        assert_eq!(
            *courier.relay().keys_minted.borrow(),
            0,
            "queueing signs nothing"
        );

        courier.relay().set_connected(true);
        courier.flush_pending(NOW + 5);
        let operation = courier.relay().last_operation();
        // The relay rejected it; the router deposits the same message again later.
        assert!(
            !courier
                .resolve_publish(operation, false, NOW + 6)
                .expect("resolves")
                .accepted
        );
        let again = courier.deposit("m1", &envelope, NOW + 7);
        assert!(matches!(again.outcome, DepositOutcome::Publishing(_)));

        let keys = courier.relay().publisher_keys();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1]);
    }

    #[test]
    fn a_publisher_key_does_not_print_itself() {
        let key = PublisherKey::from_bytes([0x7Au8; 32]);
        assert_eq!(format!("{key:?}"), "PublisherKey(<redacted>)");
        assert!(!format!("{key:?}").contains("122"));
    }

    // --- Adjacent-day tags -------------------------------------------------

    #[test]
    fn watches_the_adjacent_day_tags() {
        use chatter_mesh::tlv::courier_envelope::{epoch_day, recipient_tag};

        let mut courier = new_courier(true);
        let key = Some([0x21u8; 32]);
        courier.refresh(key, NOW);

        let day = epoch_day(NOW);
        let mine = courier.my_tags().to_vec();
        assert_eq!(mine.len(), 3);
        for candidate in [day - 1, day, day + 1] {
            assert!(
                mine.contains(&recipient_tag(&[0x21u8; 32], candidate)),
                "day {candidate} must be watched"
            );
        }
    }

    #[test]
    fn a_drop_published_before_utc_midnight_is_still_findable_after_it() {
        use chatter_mesh::tlv::courier_envelope::{epoch_day, recipient_tag};

        let key = [0x22u8; 32];
        // One second before a UTC midnight, and one second after it.
        let before = (NOW / DAY + 1) * DAY - 1;
        let after = before + 2;
        assert_eq!(epoch_day(after), epoch_day(before) + 1, "the day must roll");

        let sender_tag = recipient_tag(&key, epoch_day(before));
        let mut courier = new_courier(true);
        let local = Some(key);

        courier.refresh(local, before);
        assert!(courier.my_tags().contains(&sender_tag));

        // After the rollover the recipient keeps yesterday's tag in the watch set,
        // which is the only reason a drop sealed a second before midnight is ever
        // found.
        courier.refresh(local, after);
        assert!(
            courier.my_tags().contains(&sender_tag),
            "yesterday's tag must survive the rollover"
        );
        assert!(
            courier
                .my_tags()
                .contains(&recipient_tag(&key, epoch_day(after)))
        );

        let envelope = CourierEnvelope::new(sender_tag, millis(after + DAY), vec![9; 16], 1, None);
        let encoded = envelope.encode().expect("encodes");
        let event = DropEvent {
            event_id: "e-rollover",
            tag: sender_tag,
            envelope_bytes: &encoded,
        };
        assert_eq!(
            courier.handle_drop(&event, after),
            DropDisposition::OpenedForSelf
        );
    }

    #[test]
    fn the_subscription_is_rebuilt_only_when_the_tag_set_changes() {
        let mut courier = new_courier(true);
        let key = Some([0x23u8; 32]);
        courier.refresh(key, NOW);
        courier.refresh(key, NOW + 60);
        assert_eq!(courier.relay().subscriptions.borrow().len(), 1);

        // A day later the tags have rotated, so the subscription must be reopened.
        courier.refresh(key, NOW + DAY);
        assert_eq!(courier.relay().subscriptions.borrow().len(), 2);
    }

    // --- The pending queue -------------------------------------------------

    #[test]
    fn the_pending_queue_drops_the_oldest_at_its_exact_bound() {
        let mut courier = new_courier(false);
        let key = [0x31u8; 32];
        let mut first_operation = None;

        for index in 0..MAX_PENDING_DROPS {
            let deposited = courier.deposit(
                &format!("m{index}"),
                &envelope_for(&key, NOW, index as u8),
                NOW,
            );
            assert!(
                deposited.abandoned.is_empty(),
                "nothing evicted below the bound"
            );
            if index == 0 {
                first_operation = match deposited.outcome {
                    DepositOutcome::Queued(op) => Some(op),
                    other => panic!("expected a queued drop, got {other:?}"),
                };
            }
        }
        assert_eq!(courier.pending_len(), MAX_PENDING_DROPS);

        // One past the bound: the oldest goes, the queue stays at its cap.
        let overflow = courier.deposit("m-overflow", &envelope_for(&key, NOW, 99), NOW);
        assert_eq!(courier.pending_len(), MAX_PENDING_DROPS);
        assert_eq!(
            overflow.abandoned,
            vec![AbandonedDrop {
                operation: first_operation.expect("first operation"),
                message_id: "m0".to_string(),
            }]
        );

        // And the evicted message is the one no longer flushed.
        courier.relay().set_connected(true);
        courier.flush_pending(NOW + 1);
        assert_eq!(courier.relay().publish_count(), MAX_PENDING_DROPS);
        assert_eq!(courier.pending_len(), 0);
    }

    #[test]
    fn a_deposit_is_idempotent_per_message_id() {
        let mut courier = new_courier(true);
        let key = [0x32u8; 32];
        let envelope = envelope_for(&key, NOW, 1);

        let first = courier.deposit("m1", &envelope, NOW);
        let DepositOutcome::Publishing(operation) = first.outcome else {
            panic!("expected a publish");
        };
        assert_eq!(
            courier.deposit("m1", &envelope, NOW).outcome,
            DepositOutcome::Refused(DepositRefusal::AlreadyInFlight)
        );

        courier
            .resolve_publish(operation, true, NOW)
            .expect("resolves");
        assert_eq!(
            courier.deposit("m1", &envelope, NOW).outcome,
            DepositOutcome::Refused(DepositRefusal::AlreadyPublished)
        );
        assert_eq!(courier.relay().publish_count(), 1);
    }

    #[test]
    fn a_rejected_relay_result_leaves_the_message_depositable_again() {
        let mut courier = new_courier(true);
        let envelope = envelope_for(&[0x33u8; 32], NOW, 1);
        let DepositOutcome::Publishing(operation) = courier.deposit("m1", &envelope, NOW).outcome
        else {
            panic!("expected a publish");
        };
        courier
            .resolve_publish(operation, false, NOW)
            .expect("resolves");
        assert!(matches!(
            courier.deposit("m1", &envelope, NOW + 1).outcome,
            DepositOutcome::Publishing(_)
        ));
    }

    #[test]
    fn a_stale_relay_callback_cannot_complete_a_newer_attempt() {
        let mut courier = new_courier(true);
        let envelope = envelope_for(&[0x34u8; 32], NOW, 1);
        let DepositOutcome::Publishing(operation) = courier.deposit("m1", &envelope, NOW).outcome
        else {
            panic!("expected a publish");
        };
        assert!(courier.resolve_publish(operation, true, NOW).is_some());
        // The same callback arriving twice, and one for an operation that was wiped.
        assert!(courier.resolve_publish(operation, true, NOW).is_none());
        assert!(
            courier
                .resolve_publish(DropOperationId(9_999), true, NOW)
                .is_none()
        );
    }

    #[test]
    fn an_oversized_envelope_is_suppressed_rather_than_retried() {
        let mut courier = new_courier(true);
        let envelope = CourierEnvelope::new(
            [0x35u8; TAG_LEN],
            millis(NOW + DAY),
            vec![7u8; MAX_DROP_ENVELOPE_BYTES + 1],
            1,
            None,
        );
        assert_eq!(
            courier.deposit("m-big", &envelope, NOW).outcome,
            DepositOutcome::Refused(DepositRefusal::Undeliverable)
        );
        // Suppressed for the process, and never recorded as if a relay took it.
        assert_eq!(
            courier.deposit("m-big", &envelope, NOW).outcome,
            DepositOutcome::Refused(DepositRefusal::Undeliverable)
        );
        assert_eq!(courier.relay().publish_count(), 0);
        assert!(courier.dedup_snapshot().published_drop_keys.is_empty());
    }

    #[test]
    fn an_expired_envelope_is_never_published() {
        let mut courier = new_courier(true);
        let expired = CourierEnvelope::new([0x36u8; TAG_LEN], millis(NOW), vec![1; 8], 1, None);
        assert_eq!(
            courier.deposit("m-old", &expired, NOW).outcome,
            DepositOutcome::Refused(DepositRefusal::Expired)
        );
        assert_eq!(courier.relay().publish_count(), 0);
    }

    #[test]
    fn nothing_is_published_while_the_bridge_is_off() {
        let mut courier = new_courier(true);
        courier.set_bridge_enabled(false);
        assert_eq!(
            courier
                .deposit("m1", &envelope_for(&[0x37u8; 32], NOW, 1), NOW)
                .outcome,
            DepositOutcome::Refused(DepositRefusal::BridgeDisabled)
        );
        assert_eq!(courier.relay().publish_count(), 0);
    }

    #[test]
    fn disabling_the_bridge_cancels_queued_and_in_flight_drops() {
        let mut courier = new_courier(true);
        let key = [0x38u8; 32];
        let DepositOutcome::Publishing(in_flight) = courier
            .deposit("m1", &envelope_for(&key, NOW, 1), NOW)
            .outcome
        else {
            panic!("expected a publish");
        };
        courier.relay().set_connected(false);
        let DepositOutcome::Queued(queued) = courier
            .deposit("m2", &envelope_for(&key, NOW, 2), NOW)
            .outcome
        else {
            panic!("expected a queue");
        };

        courier.set_bridge_enabled(false);
        let abandoned = courier.refresh(Some(key), NOW);
        let operations: Vec<DropOperationId> =
            abandoned.iter().map(|drop| drop.operation).collect();
        assert!(operations.contains(&in_flight));
        assert!(operations.contains(&queued));
        // A late relay callback for a cancelled operation finds nothing.
        assert!(courier.resolve_publish(in_flight, true, NOW).is_none());
        assert_eq!(courier.pending_len(), 0);
    }

    #[test]
    fn the_drop_carries_the_envelope_tag_and_a_nip40_expiry() {
        let mut courier = new_courier(true);
        let key = [0x39u8; 32];
        let envelope = envelope_for(&key, NOW, 5);
        courier.deposit("m1", &envelope, NOW);

        let published = courier.relay().published.borrow();
        let drop = published.first().expect("a drop");
        assert_eq!(drop.tag, envelope.recipient_tag);
        assert_eq!(drop.envelope, envelope.encode().expect("encodes"));
        assert_eq!(drop.expires_at, envelope.expiry_ms / 1_000);
        assert!(drop.expires_at > NOW, "NIP-40 must be in the future");
    }

    // --- Inbound drops -----------------------------------------------------

    #[test]
    fn a_drop_for_an_unknown_tag_is_ignored_rather_than_opened() {
        let mut courier = new_courier(true);
        courier.refresh(Some([0x61u8; 32]), NOW);

        // Someone else's mail, addressed to a tag we do not watch.
        let envelope = envelope_for(&[0x62u8; 32], NOW, 9);
        let encoded = envelope.encode().expect("encodes");
        let disposition = courier.handle_drop(
            &DropEvent {
                event_id: "e-stranger",
                tag: envelope.recipient_tag,
                envelope_bytes: &encoded,
            },
            NOW,
        );
        assert_eq!(
            disposition,
            DropDisposition::Ignored(IgnoreReason::NotWatched)
        );
        assert!(
            courier.sink().opened.borrow().is_empty(),
            "an unwatched tag must not reach the seal at all"
        );
        assert!(courier.dedup_snapshot().seen_drop_event_ids.is_empty());
    }

    #[test]
    fn a_mislabelled_drop_cannot_ride_someone_elses_subscription() {
        let mut courier = new_courier(true);
        let key = [0x63u8; 32];
        courier.refresh(Some(key), NOW);
        let our_tag = courier.my_tags()[1];

        // The event claims our tag; the envelope inside is addressed elsewhere.
        let envelope = envelope_for(&[0x64u8; 32], NOW, 10);
        let encoded = envelope.encode().expect("encodes");
        let disposition = courier.handle_drop(
            &DropEvent {
                event_id: "e-mislabelled",
                tag: our_tag,
                envelope_bytes: &encoded,
            },
            NOW,
        );
        assert_eq!(
            disposition,
            DropDisposition::Ignored(IgnoreReason::Mislabelled)
        );
        assert!(courier.sink().opened.borrow().is_empty());
    }

    #[test]
    fn the_same_drop_from_two_relays_is_opened_once() {
        let mut courier = new_courier(true);
        let key = [0x65u8; 32];
        courier.refresh(Some(key), NOW);
        let envelope = envelope_for(&key, NOW, 11);
        let encoded = envelope.encode().expect("encodes");
        let event = DropEvent {
            event_id: "e-dup",
            tag: envelope.recipient_tag,
            envelope_bytes: &encoded,
        };

        assert_eq!(
            courier.handle_drop(&event, NOW),
            DropDisposition::OpenedForSelf
        );
        assert_eq!(
            courier.handle_drop(&event, NOW),
            DropDisposition::Ignored(IgnoreReason::AlreadyHandled)
        );
        assert_eq!(courier.sink().opened.borrow().len(), 1);
    }

    #[test]
    fn a_transient_sink_failure_leaves_the_drop_retryable() {
        let mut courier = new_courier(true);
        let key = [0x66u8; 32];
        courier.refresh(Some(key), NOW);
        *courier.sink().accept_open.borrow_mut() = false;

        let envelope = envelope_for(&key, NOW, 12);
        let encoded = envelope.encode().expect("encodes");
        let event = DropEvent {
            event_id: "e-retry",
            tag: envelope.recipient_tag,
            envelope_bytes: &encoded,
        };
        assert_eq!(courier.handle_drop(&event, NOW), DropDisposition::Retryable);
        assert!(courier.dedup_snapshot().seen_drop_event_ids.is_empty());

        *courier.sink().accept_open.borrow_mut() = true;
        assert_eq!(
            courier.handle_drop(&event, NOW),
            DropDisposition::OpenedForSelf
        );
    }

    #[test]
    fn malformed_and_expired_drops_go_no_further() {
        let mut courier = new_courier(true);
        let key = [0x67u8; 32];
        courier.refresh(Some(key), NOW);

        assert_eq!(
            courier.handle_drop(
                &DropEvent {
                    event_id: "e-garbage",
                    tag: [0u8; TAG_LEN],
                    envelope_bytes: &[0xFF, 0x00],
                },
                NOW,
            ),
            DropDisposition::Ignored(IgnoreReason::Malformed)
        );

        assert_eq!(
            courier.handle_drop(
                &DropEvent {
                    event_id: "e-huge",
                    tag: [0u8; TAG_LEN],
                    envelope_bytes: &vec![0u8; MAX_DROP_ENVELOPE_BYTES + 1],
                },
                NOW,
            ),
            DropDisposition::Ignored(IgnoreReason::Malformed)
        );

        let expired =
            CourierEnvelope::new(courier.my_tags()[1], millis(NOW - 1), vec![1; 8], 1, None);
        let encoded = expired.encode().expect("encodes");
        assert_eq!(
            courier.handle_drop(
                &DropEvent {
                    event_id: "e-expired",
                    tag: expired.recipient_tag,
                    envelope_bytes: &encoded,
                },
                NOW,
            ),
            DropDisposition::Ignored(IgnoreReason::Expired)
        );
        assert!(courier.sink().opened.borrow().is_empty());
    }

    #[test]
    fn drops_are_ignored_while_the_bridge_is_off() {
        let mut courier = new_courier(true);
        let key = [0x68u8; 32];
        courier.refresh(Some(key), NOW);
        courier.set_bridge_enabled(false);

        let envelope = envelope_for(&key, NOW, 13);
        let encoded = envelope.encode().expect("encodes");
        assert_eq!(
            courier.handle_drop(
                &DropEvent {
                    event_id: "e-off",
                    tag: envelope.recipient_tag,
                    envelope_bytes: &encoded,
                },
                NOW,
            ),
            DropDisposition::Ignored(IgnoreReason::BridgeDisabled)
        );
    }

    // --- Refresh scheduling ------------------------------------------------

    #[test]
    fn the_refresh_falls_due_every_half_hour() {
        let mut courier = new_courier(true);
        courier.refresh(Some([0x71u8; 32]), NOW);
        assert_eq!(courier.next_refresh_due(), NOW + REFRESH_INTERVAL_SECS);
    }

    #[test]
    fn a_disconnected_refresh_closes_the_subscription_and_publishes_nothing() {
        let mut courier = new_courier(true);
        let key = Some([0x73u8; 32]);
        courier.refresh(key, NOW);
        assert!(courier.is_subscribed());

        courier.relay().set_connected(false);
        courier.refresh(key, NOW + 1);
        assert!(!courier.is_subscribed());
        assert_eq!(*courier.relay().closes.borrow(), 1);
        assert_eq!(courier.relay().publish_count(), 0);
    }

    #[test]
    fn with_no_key_there_is_nothing_to_subscribe_to() {
        let mut courier = new_courier(true);
        courier.refresh(None, NOW);
        assert!(!courier.is_subscribed());
        assert!(courier.relay().subscriptions.borrow().is_empty());
    }

    // --- Dedup record ------------------------------------------------------

    #[test]
    fn the_dedup_record_survives_a_restart() {
        let mut courier = new_courier(true);
        let key = [0x81u8; 32];
        let envelope = envelope_for(&key, NOW, 14);
        let DepositOutcome::Publishing(operation) = courier.deposit("m1", &envelope, NOW).outcome
        else {
            panic!("expected a publish");
        };
        courier
            .resolve_publish(operation, true, NOW)
            .expect("resolves");
        let snapshot = courier.dedup_snapshot();
        assert_eq!(snapshot.published_drop_keys.len(), 1);

        // A fresh process reloads the record and refuses to republish the same mail,
        // which is what stops a relaunch from re-flooding the relays.
        let mut restarted = new_courier(true);
        restarted.restore_dedup(&snapshot, NOW + 60);
        assert_eq!(
            restarted.deposit("m1", &envelope, NOW + 60).outcome,
            DepositOutcome::Refused(DepositRefusal::AlreadyPublished)
        );

        // A record older than the drop window is not restored: the relay has dropped
        // the event by then, so republishing is correct again.
        let mut much_later = new_courier(true);
        much_later.restore_dedup(&snapshot, NOW + TRACKED_ID_LIFETIME_SECS + 1);
        assert!(matches!(
            much_later
                .deposit(
                    "m1",
                    &envelope_for(&key, NOW + TRACKED_ID_LIFETIME_SECS + 1, 15),
                    NOW + TRACKED_ID_LIFETIME_SECS + 1
                )
                .outcome,
            DepositOutcome::Publishing(_)
        ));
    }

    #[test]
    fn a_wipe_forgets_queued_drops_and_the_dedup_record() {
        let mut courier = new_courier(true);
        let key = [0x82u8; 32];
        let DepositOutcome::Publishing(operation) = courier
            .deposit("m1", &envelope_for(&key, NOW, 16), NOW)
            .outcome
        else {
            panic!("expected a publish");
        };
        courier
            .resolve_publish(operation, true, NOW)
            .expect("resolves");
        courier.relay().set_connected(false);
        courier.deposit("m2", &envelope_for(&key, NOW, 17), NOW);

        let abandoned = courier.wipe();
        assert_eq!(abandoned.len(), 1, "the queued drop is abandoned");
        assert_eq!(courier.dedup_snapshot(), DedupSnapshot::default());
        assert_eq!(courier.pending_len(), 0);
        assert!(courier.my_tags().is_empty());
    }

    // --- The expiring set ---------------------------------------------------

    #[test]
    fn the_id_set_ages_out_and_stays_bounded() {
        let mut set = ExpiringIdSet::new(3, 100);
        set.insert("a", 0);
        set.insert("b", 10);
        assert!(set.contains("a", 50));
        assert!(
            !set.contains("a", 100),
            "exactly at the lifetime is expired"
        );

        set.insert("c", 20);
        set.insert("d", 30);
        assert_eq!(set.len(), 3);
        assert!(
            !set.contains("a", 30),
            "the oldest was evicted at the bound"
        );
        assert!(set.contains("d", 30));

        // Re-inserting refreshes rather than duplicating.
        set.insert("d", 40);
        assert_eq!(set.len(), 3);
        assert!(set.contains("d", 139));
    }
}
