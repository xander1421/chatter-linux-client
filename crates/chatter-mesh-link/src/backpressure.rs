//! Write backpressure buffering — outbound writes, outbound notifications, and
//! inbound reassembly.
//!
//! BLE controllers refuse writes as a matter of course. `canSendWriteWithoutResponse`
//! goes false, `updateValue` returns false, and there is no retransmission anywhere
//! below us: a rejected write that the caller discards is a packet that simply never
//! existed. So rejection has to mean *queue and retry*, which immediately raises the
//! question this module exists to answer — what stops the queue from eating the
//! process when the peer on the other end just stops reading?
//!
//! Three answers, one per direction of pressure:
//!
//! - [`OutboundWriteBuffer`] — per-link, **byte**-capped, priority-ordered. Over the
//!   cap it trims from the low-priority tail, so a stalled file transfer cannot
//!   starve a handshake. Keyed by [`LinkId`] and dropped wholesale on disconnect,
//!   because rotating link identities would otherwise each mint a fresh cap.
//! - [`OutboundNotificationBuffer`] — the peripheral-role retry queue, **count**-capped,
//!   with [`NotificationRetry`] driving bounded linear backoff on top of it.
//! - [`InboundAssembler`] — per-link reassembly of offset-addressed writes, with a
//!   hard byte cap *and* a stall window, because a peer that opens a large write and
//!   goes quiet pins that memory until something reclaims it.
//!
//! Sans-IO throughout: no clocks, no timers, no async. Time enters as a
//! `now_unix_millis` parameter and leaves as a `due_at_unix_millis` the caller is
//! responsible for honouring.
//!
//! # Reporting rejection
//!
//! The two outbound buffers report a refusal differently, deliberately.
//!
//! [`OutboundNotificationBuffer::enqueue`] hands the payload *back* in
//! [`NotificationEnqueue::Full`]. bitchat's `enqueuePendingNotificationIfAccepted` is
//! the commit point of a destructive courier handoff — the caller may hold the only
//! copy of that envelope — so admission has to be a value it cannot ignore, not a
//! silent drop. Returning the bytes makes losing them require effort.
//!
//! [`OutboundWriteBuffer`] reports with [`WriteAdmission::accepted`] instead, matching
//! `writeOrEnqueueIfAccepted`: trimming can evict items *other* than the one just
//! offered, so no single returned payload could describe the outcome honestly. A
//! caller with durable upstream state keeps it retryable on `accepted == false`.
//!
//! Ported from `BLEOutboundWriteBuffer.swift`, `BLEInboundWriteBuffer.swift`,
//! `BLEOutboundNotificationBuffer.swift`, the retry loop in `BLEService.swift`
//! (`enqueuePendingNotification` / `writeOrEnqueue`), and the stall window from
//! `NotificationStreamAssembler.swift`.

use std::collections::BTreeMap;

use chatter_mesh::BitchatPacket;
use chatter_mesh::codec;

use crate::error::LinkError;
use crate::link::LinkId;

// ---------------------------------------------------------------------------
// Constants — bitchat `Services/TransportConfig.swift`
// ---------------------------------------------------------------------------

/// Per-link ceiling on queued outbound write bytes.
///
/// One megabyte per link, not process-wide: the cap is what a *single* misbehaving
/// peer can cost us, and links are already bounded by the connection budget.
pub const PENDING_WRITE_BUFFER_CAP_BYTES: usize = 1_000_000;

/// Ceiling on notifications held for retry, counted rather than measured in bytes —
/// each one is already MTU-bounded, so the count is the meaningful quantity.
pub const PENDING_NOTIFICATIONS_CAP_COUNT: usize = 128;

/// Base retry delay. Backoff is `delay * attempt`, i.e. linear, not exponential:
/// the controller queue drains in milliseconds and exponential backoff would spend
/// most of its time idle after the link recovered.
pub const NOTIFICATION_RETRY_DELAY_MS: u64 = 25;

/// Retries before the packet is dropped loudly. With linear backoff this is roughly
/// 80 seconds of trying, which is far longer than any real controller stall.
pub const NOTIFICATION_RETRY_MAX_ATTEMPTS: u32 = 80;

/// Hard ceiling on one link's partially assembled inbound frame.
pub const NOTIFICATION_ASSEMBLER_HARD_CAP_BYTES: usize = 8 * 1024 * 1024;

/// How long a partial frame may sit without progress before it is discarded.
pub const ASSEMBLER_STALL_RESET_MS: u64 = 250;

/// Log every event, then every Nth — backpressure is bursty and unsampled logging
/// of it is its own denial of service.
pub const BACKPRESSURE_LOG_INTERVAL: u64 = 25;

// ---------------------------------------------------------------------------
// Which failures mean "buffer it"
// ---------------------------------------------------------------------------

/// Whether a failed send should be queued for retry rather than discarded.
///
/// Only [`LinkError::WriteRejected`] is backpressure. [`LinkError::LinkGone`] means
/// the queue for that link should be *discarded*, not grown, and
/// [`LinkError::PayloadTooLarge`] will fail identically on every retry — buffering
/// either one converts a clear failure into an unbounded one.
#[must_use]
pub fn should_buffer(error: &LinkError) -> bool {
    matches!(error, LinkError::WriteRejected(_))
}

// ---------------------------------------------------------------------------
// Outbound write buffer
// ---------------------------------------------------------------------------

/// Ordering among queued writes on one link.
///
/// Two fields rather than one so that fragments of a *small* packet outrank
/// fragments of a large one: when the cap forces a trim, finishing a nearly complete
/// transfer beats making equal partial progress on everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WritePriority {
    /// Primary class. Lower goes out first.
    pub level: u8,
    /// Tie-break within a class. Lower goes out first.
    pub suborder: u32,
}

impl WritePriority {
    /// Handshakes, announces, leaves — anything whose loss stalls the mesh itself.
    pub const HIGH: Self = Self {
        level: 0,
        suborder: 0,
    };

    /// Bulk file payloads: below every fragment, above nothing but [`Self::LOW`].
    pub const FILE_TRANSFER: Self = Self {
        level: 2,
        suborder: u32::MAX - 1,
    };

    /// The first thing trimmed when the cap is hit.
    pub const LOW: Self = Self {
        level: 2,
        suborder: u32::MAX,
    };

    /// A fragment belonging to a transfer of `total_fragments` pieces.
    ///
    /// Clamped to `1..=u16::MAX` exactly as bitchat does — a caller reporting zero or
    /// an absurd total must not be able to reorder the whole queue by it.
    #[must_use]
    pub fn fragment(total_fragments: usize) -> Self {
        let clamped = total_fragments.clamp(1, u16::MAX as usize);
        Self {
            level: 1,
            suborder: clamped as u32,
        }
    }
}

/// One write waiting for the controller to accept it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingWrite {
    pub priority: WritePriority,
    pub bytes: Vec<u8>,
}

/// What the buffer did with an offered write.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub enum WriteEnqueue {
    /// Accepted into the queue. `trimmed_bytes` is what the cap forced out to make
    /// room — possibly including the offered write itself, which is why
    /// [`WriteAdmission::accepted`] exists.
    Enqueued {
        trimmed_bytes: usize,
        remaining_bytes: usize,
    },
    /// Larger than the whole per-link cap. Never queued: it could not fit even in an
    /// empty queue, so buffering it would only evict writes that can still be sent.
    Oversized { offered_bytes: usize },
}

/// [`WriteEnqueue`] plus whether the *offered* write survived trimming.
///
/// `Enqueued` alone cannot express that: a full queue may admit the new item and
/// immediately trim it back off if it sorted to the tail.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub struct WriteAdmission {
    pub result: WriteEnqueue,
    /// True only if the offered write is still queued after trimming.
    pub accepted: bool,
}

/// Per-link, byte-capped, priority-ordered outbound writes.
#[derive(Clone, Debug, Default)]
pub struct OutboundWriteBuffer {
    writes: BTreeMap<LinkId, Vec<PendingWrite>>,
}

impl OutboundWriteBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Links with at least one queued write, in deterministic order.
    pub fn link_ids(&self) -> impl Iterator<Item = &LinkId> + '_ {
        self.writes.keys()
    }

    /// Number of links holding queued writes.
    #[must_use]
    pub fn link_count(&self) -> usize {
        self.writes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// Writes queued for one link.
    #[must_use]
    pub fn queued_count(&self, link: &LinkId) -> usize {
        self.writes.get(link).map_or(0, Vec::len)
    }

    /// Bytes queued for one link. Never exceeds the `cap_bytes` last used on it.
    #[must_use]
    pub fn queued_bytes(&self, link: &LinkId) -> usize {
        self.writes
            .get(link)
            .map_or(0, |queue| queue.iter().map(|w| w.bytes.len()).sum())
    }

    /// Bytes queued across every link. Bounded by `links * cap_bytes`, which is why
    /// [`Self::discard_all`] on disconnect is not optional.
    #[must_use]
    pub fn total_queued_bytes(&self) -> usize {
        self.writes
            .values()
            .flat_map(|queue| queue.iter())
            .map(|w| w.bytes.len())
            .sum()
    }

    /// Drops every queue on every link.
    pub fn clear(&mut self) {
        self.writes.clear();
    }

    /// Queues a write, ignoring whether that specific write survived.
    ///
    /// Use [`Self::enqueue_reporting_acceptance`] wherever the caller's own state
    /// depends on the answer.
    pub fn enqueue(
        &mut self,
        link: &LinkId,
        bytes: Vec<u8>,
        priority: WritePriority,
        cap_bytes: usize,
    ) -> WriteEnqueue {
        self.enqueue_reporting_acceptance(link, bytes, priority, cap_bytes)
            .result
    }

    /// Queues a write and reports whether it is still queued afterwards.
    pub fn enqueue_reporting_acceptance(
        &mut self,
        link: &LinkId,
        bytes: Vec<u8>,
        priority: WritePriority,
        cap_bytes: usize,
    ) -> WriteAdmission {
        let offered_bytes = bytes.len();
        if offered_bytes > cap_bytes {
            return WriteAdmission {
                result: WriteEnqueue::Oversized { offered_bytes },
                accepted: false,
            };
        }

        // Taken out and put back so the insert and the trim see one owned queue.
        let mut queue = self.writes.remove(link).unwrap_or_default();

        // First strictly-lower-priority item, or the tail. Ties keep FIFO order,
        // which matters: fragments of one transfer share a priority and must not be
        // reordered relative to each other.
        let insert_index = queue
            .iter()
            .position(|existing| priority < existing.priority)
            .unwrap_or(queue.len());
        queue.insert(insert_index, PendingWrite { priority, bytes });

        let mut total: usize = queue.iter().map(|w| w.bytes.len()).sum();
        let mut trimmed_bytes = 0usize;
        while total > cap_bytes {
            match queue.pop() {
                Some(removed) => {
                    trimmed_bytes += removed.bytes.len();
                    total -= removed.bytes.len();
                }
                None => break,
            }
        }

        // Trimming only ever removes from the tail, so the offered write survives
        // exactly when its insertion index is still in range.
        let accepted = insert_index < queue.len();

        if !queue.is_empty() {
            self.writes.insert(link.clone(), queue);
        }

        WriteAdmission {
            result: WriteEnqueue::Enqueued {
                trimmed_bytes,
                remaining_bytes: total,
            },
            accepted,
        }
    }

    /// Removes and returns everything queued for a link, highest priority first.
    #[must_use]
    pub fn take_all(&mut self, link: &LinkId) -> Vec<PendingWrite> {
        self.writes.remove(link).unwrap_or_default()
    }

    /// Drops a link's queue without returning it.
    ///
    /// Called on disconnect. Keeping the entry would let rotating link identities
    /// accumulate a fresh per-link byte cap each, which is unbounded growth wearing
    /// a bounded cap's clothing — and the ciphertext is link-specific anyway, so it
    /// is useless to whatever link replaces this one.
    pub fn discard_all(&mut self, link: &LinkId) {
        self.writes.remove(link);
    }

    /// Puts partially drained writes back at the head, preserving their order.
    ///
    /// Deliberately does not re-apply the cap: these bytes were already admitted and
    /// were about to go out on the wire. Re-trimming here would drop the oldest,
    /// highest-priority items in the system.
    pub fn prepend(&mut self, link: &LinkId, items: Vec<PendingWrite>) {
        if items.is_empty() {
            return;
        }
        let mut queue = items;
        if let Some(existing) = self.writes.remove(link) {
            queue.extend(existing);
        }
        self.writes.insert(link.clone(), queue);
    }
}

// ---------------------------------------------------------------------------
// Outbound notification buffer
// ---------------------------------------------------------------------------

/// A notification awaiting a subscriber-ready callback.
///
/// `targets` of `None` means broadcast to whoever is subscribed at send time; `Some`
/// pins it to specific subscribers, which is what makes per-target ciphertext safe
/// to hold across a resubscribe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingNotification<T> {
    pub bytes: Vec<u8>,
    pub targets: Option<Vec<T>>,
}

/// What the notification buffer did with an offered packet.
///
/// `Full` carries the payload back out. That is the whole point: a caller that has
/// already destroyed its own copy — a courier handoff committing on this result —
/// must not be able to lose the packet by ignoring a return value.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub enum NotificationEnqueue<T> {
    /// Accepted. `depth` is the queue length including this packet.
    Enqueued { depth: usize },
    /// Rejected: the queue is at its count cap. The packet is returned unmodified.
    Full {
        depth: usize,
        bytes: Vec<u8>,
        targets: Option<Vec<T>>,
    },
}

impl<T> NotificationEnqueue<T> {
    #[must_use]
    pub fn accepted(&self) -> bool {
        matches!(self, Self::Enqueued { .. })
    }

    /// Queue depth after the attempt, whether or not it succeeded.
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::Enqueued { depth } | Self::Full { depth, .. } => *depth,
        }
    }
}

/// Count-capped FIFO of notifications the peripheral role could not send yet.
#[derive(Clone, Debug)]
pub struct OutboundNotificationBuffer<T> {
    pending: Vec<PendingNotification<T>>,
}

impl<T> Default for OutboundNotificationBuffer<T> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
        }
    }
}

impl<T> OutboundNotificationBuffer<T> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Whether another packet would be refused at this cap.
    #[must_use]
    pub fn is_full(&self, cap_count: usize) -> bool {
        self.pending.len() >= cap_count
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Admits a packet if there is room, and hands it back if there is not.
    pub fn enqueue(
        &mut self,
        bytes: Vec<u8>,
        targets: Option<Vec<T>>,
        cap_count: usize,
    ) -> NotificationEnqueue<T> {
        if self.pending.len() >= cap_count {
            return NotificationEnqueue::Full {
                depth: self.pending.len(),
                bytes,
                targets,
            };
        }
        self.pending.push(PendingNotification { bytes, targets });
        NotificationEnqueue::Enqueued {
            depth: self.pending.len(),
        }
    }

    /// Removes and returns everything queued, oldest first.
    #[must_use]
    pub fn take_all(&mut self) -> Vec<PendingNotification<T>> {
        std::mem::take(&mut self.pending)
    }

    /// Puts undelivered notifications back at the head after a partial drain.
    ///
    /// Uncapped for the same reason as [`OutboundWriteBuffer::prepend`]: these were
    /// admitted already, and re-capping here would drop the oldest packets in favour
    /// of the newest.
    pub fn prepend(&mut self, items: Vec<PendingNotification<T>>) {
        if items.is_empty() {
            return;
        }
        let mut merged = items;
        merged.append(&mut self.pending);
        self.pending = merged;
    }

    /// Drops a departed subscriber from target-specific retries.
    ///
    /// Broadcast entries (`targets == None`) survive — they are still valid for
    /// whoever remains subscribed. A targeted entry whose last target left is
    /// discarded outright: its ciphertext is readable by nobody else.
    pub fn remove_target(&mut self, matches: impl Fn(&T) -> bool) {
        self.pending.retain_mut(|notification| {
            let Some(targets) = notification.targets.as_mut() else {
                return true;
            };
            targets.retain(|target| !matches(target));
            !targets.is_empty()
        });
    }
}

// ---------------------------------------------------------------------------
// Bounded retry
// ---------------------------------------------------------------------------

/// Backoff shape and the attempt ceiling for notification retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationRetryPolicy {
    pub delay_ms: u64,
    pub max_attempts: u32,
}

impl Default for NotificationRetryPolicy {
    fn default() -> Self {
        Self {
            delay_ms: NOTIFICATION_RETRY_DELAY_MS,
            max_attempts: NOTIFICATION_RETRY_MAX_ATTEMPTS,
        }
    }
}

impl NotificationRetryPolicy {
    /// Delay before the `attempt`th retry. Linear: `delay_ms * attempt`, floored at
    /// one interval so attempt 0 never schedules for *now* and spins.
    #[must_use]
    pub fn backoff_ms(&self, attempt: u32) -> u64 {
        self.delay_ms.saturating_mul(u64::from(attempt.max(1)))
    }
}

/// One packet being retried into an [`OutboundNotificationBuffer`].
///
/// The Swift original lived in a chain of `asyncAfter` closures, which is exactly
/// what a sans-IO port cannot have. The state those closures captured — payload,
/// attempt count, next deadline — becomes this value, and the caller owns the timer.
/// A retry that is never polled again holds exactly one packet: it cannot grow.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub struct NotificationRetry<T> {
    bytes: Vec<u8>,
    targets: Option<Vec<T>>,
    attempt: u32,
    due_at_unix_millis: u64,
}

impl<T> NotificationRetry<T> {
    /// Starts a retry that is due immediately.
    pub fn new(bytes: Vec<u8>, targets: Option<Vec<T>>, now_unix_millis: u64) -> Self {
        Self {
            bytes,
            targets,
            attempt: 0,
            due_at_unix_millis: now_unix_millis,
        }
    }

    /// Admission attempts already made.
    #[must_use]
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    #[must_use]
    pub fn due_at_unix_millis(&self) -> u64 {
        self.due_at_unix_millis
    }

    #[must_use]
    pub fn is_due(&self, now_unix_millis: u64) -> bool {
        now_unix_millis >= self.due_at_unix_millis
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn targets(&self) -> Option<&[T]> {
        self.targets.as_deref()
    }

    /// Tries once more to hand the packet to `buffer`.
    ///
    /// Polling before [`Self::due_at_unix_millis`] is a no-op that consumes no
    /// attempt. Without that, a caller looping without a timer would burn the entire
    /// retry budget in microseconds and drop a packet the controller was about to
    /// accept — the one thing GCD's `asyncAfter` guaranteed for free.
    pub fn poll(
        self,
        buffer: &mut OutboundNotificationBuffer<T>,
        policy: &NotificationRetryPolicy,
        cap_count: usize,
        now_unix_millis: u64,
    ) -> RetryStep<T> {
        let Self {
            bytes,
            targets,
            attempt,
            due_at_unix_millis,
        } = self;

        if now_unix_millis < due_at_unix_millis {
            return RetryStep::Waiting(Self {
                bytes,
                targets,
                attempt,
                due_at_unix_millis,
            });
        }

        match buffer.enqueue(bytes, targets, cap_count) {
            NotificationEnqueue::Enqueued { depth } => RetryStep::Admitted {
                depth,
                attempts: attempt.saturating_add(1),
            },
            NotificationEnqueue::Full {
                depth,
                bytes,
                targets,
            } => {
                if attempt >= policy.max_attempts {
                    return RetryStep::Dropped {
                        bytes,
                        targets,
                        attempts: attempt,
                        depth,
                    };
                }
                let next = attempt.saturating_add(1);
                RetryStep::Waiting(Self {
                    bytes,
                    targets,
                    attempt: next,
                    due_at_unix_millis: now_unix_millis.saturating_add(policy.backoff_ms(next)),
                })
            }
        }
    }
}

/// Outcome of one [`NotificationRetry::poll`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub enum RetryStep<T> {
    /// Handed to the buffer. Nothing further to do.
    Admitted { depth: usize, attempts: u32 },
    /// Still refused, or not yet due. Poll again at
    /// [`NotificationRetry::due_at_unix_millis`].
    Waiting(NotificationRetry<T>),
    /// Retry window exhausted. The packet is in no queue and is returned here.
    ///
    /// This is the loud drop: the caller is expected to log it at error level, and
    /// the payload comes back so it can be described or re-sourced. Silently
    /// discarding a `Dropped` is a lost packet with no record of it.
    Dropped {
        bytes: Vec<u8>,
        targets: Option<Vec<T>>,
        attempts: u32,
        depth: usize,
    },
}

// ---------------------------------------------------------------------------
// Inbound assembly
// ---------------------------------------------------------------------------

/// Bounds on inbound reassembly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboundLimits {
    /// Hard ceiling on one link's partial frame.
    pub cap_bytes: usize,
    /// How long a partial frame may sit without progress before it is discarded.
    pub stall_reset_ms: u64,
}

impl Default for InboundLimits {
    fn default() -> Self {
        Self {
            cap_bytes: NOTIFICATION_ASSEMBLER_HARD_CAP_BYTES,
            stall_reset_ms: ASSEMBLER_STALL_RESET_MS,
        }
    }
}

/// One offset-addressed piece of an inbound write.
///
/// GATT long writes arrive as prepared requests carrying explicit offsets, and they
/// are not required to arrive in order or to be contiguous — hence the offset rather
/// than a plain append.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundChunk {
    pub offset: usize,
    pub bytes: Vec<u8>,
}

/// What one [`InboundAssembler::append`] observed, regardless of outcome.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct InboundAppendMetadata {
    /// Size of the assembled buffer after this append.
    pub accumulated_bytes: usize,
    /// Bytes contributed by this call.
    pub appended_bytes: usize,
    /// Offsets seen in this call, in the order given.
    pub offsets: Vec<usize>,
    /// Byte 1 of the assembled buffer — the packet type — once at least two bytes
    /// exist. Useful for logging a frame that never completes.
    pub packet_type: Option<u8>,
    /// Bytes thrown away because the previous partial frame stalled past the window.
    /// Non-zero means reassembly restarted from this call's chunks.
    pub stall_discarded_bytes: usize,
}

/// Result of appending chunks for one link.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub enum InboundAppend {
    /// A complete packet parsed out; the link's buffer is now empty.
    Decoded {
        packet: Box<BitchatPacket>,
        metadata: InboundAppendMetadata,
    },
    /// Not a whole frame yet. Held, subject to the cap and the stall window.
    Waiting { metadata: InboundAppendMetadata },
    /// The write would exceed the hard cap. The link's buffer is dropped, so the
    /// peer can start a fresh frame but cannot pin memory with this one.
    Oversized { metadata: InboundAppendMetadata },
}

/// A partial frame reclaimed by [`InboundAssembler::reap_stalled`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StalledPartial {
    pub link: LinkId,
    pub discarded_bytes: usize,
    pub idle_ms: u64,
}

#[derive(Clone, Debug)]
struct Partial {
    bytes: Vec<u8>,
    updated_at_unix_millis: u64,
}

/// Per-link reassembly of offset-addressed inbound writes.
///
/// Two independent bounds, because they close different holes. The byte cap stops a
/// peer streaming unbounded data into one frame. The stall window stops a peer that
/// writes *one byte* and then goes silent forever — cheap for them, permanent for
/// us, and entirely invisible to a cap that is never approached.
#[derive(Clone, Debug, Default)]
pub struct InboundAssembler {
    partials: BTreeMap<LinkId, Partial>,
}

impl InboundAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Links currently holding a partial frame.
    #[must_use]
    pub fn link_count(&self) -> usize {
        self.partials.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.partials.is_empty()
    }

    /// Bytes held for one link.
    #[must_use]
    pub fn buffered_bytes(&self, link: &LinkId) -> usize {
        self.partials.get(link).map_or(0, |p| p.bytes.len())
    }

    /// Bytes held across every link.
    #[must_use]
    pub fn total_buffered_bytes(&self) -> usize {
        self.partials.values().map(|p| p.bytes.len()).sum()
    }

    /// Drops every partial frame.
    pub fn clear(&mut self) {
        self.partials.clear();
    }

    /// Drops one link's partial frame. Call on disconnect.
    pub fn discard(&mut self, link: &LinkId) -> usize {
        self.partials.remove(link).map_or(0, |p| p.bytes.len())
    }

    /// Applies chunks to a link's buffer and tries to parse a packet out of it.
    ///
    /// A stalled prior frame is discarded *before* the chunks land, so a peer
    /// resuming after the window starts clean rather than splicing new bytes into a
    /// frame nobody is still waiting on.
    pub fn append(
        &mut self,
        link: &LinkId,
        chunks: &[InboundChunk],
        limits: InboundLimits,
        now_unix_millis: u64,
    ) -> InboundAppend {
        let mut stall_discarded_bytes = 0usize;
        if let Some(existing) = self.partials.get(link) {
            let idle = now_unix_millis.saturating_sub(existing.updated_at_unix_millis);
            if idle >= limits.stall_reset_ms && !existing.bytes.is_empty() {
                stall_discarded_bytes = existing.bytes.len();
                self.partials.remove(link);
            }
        }

        let mut combined = self
            .partials
            .remove(link)
            .map_or_else(Vec::new, |p| p.bytes);

        let mut appended_bytes = 0usize;
        let mut offsets = Vec::new();
        let mut required_bytes = combined.len();
        let mut overflows = false;

        // Bound-check every chunk *before* touching the buffer. bitchat zero-fills to
        // `offset + len` and only then compares against the cap, which lets a peer
        // ask for a terabyte allocation with a 20-byte write. Checking first means a
        // hostile offset costs nothing.
        for chunk in chunks.iter().filter(|c| !c.bytes.is_empty()) {
            offsets.push(chunk.offset);
            appended_bytes += chunk.bytes.len();
            match chunk.offset.checked_add(chunk.bytes.len()) {
                Some(end) => required_bytes = required_bytes.max(end),
                None => overflows = true,
            }
        }

        if overflows || required_bytes > limits.cap_bytes {
            // Buffer already removed above; nothing to clean up.
            return InboundAppend::Oversized {
                metadata: InboundAppendMetadata {
                    accumulated_bytes: required_bytes,
                    appended_bytes,
                    offsets,
                    packet_type: combined.get(1).copied(),
                    stall_discarded_bytes,
                },
            };
        }

        for chunk in chunks.iter().filter(|c| !c.bytes.is_empty()) {
            let end = chunk.offset.saturating_add(chunk.bytes.len());
            if combined.len() < end {
                // Gaps are zero-filled: a later chunk may still fill them, and a
                // frame that stays holed simply never parses.
                combined.resize(end, 0);
            }
            if let Some(slot) = combined.get_mut(chunk.offset..end) {
                slot.copy_from_slice(&chunk.bytes);
            }
        }

        let metadata = InboundAppendMetadata {
            accumulated_bytes: combined.len(),
            appended_bytes,
            offsets,
            packet_type: combined.get(1).copied(),
            stall_discarded_bytes,
        };

        if let Ok(packet) = codec::decode(&combined) {
            return InboundAppend::Decoded {
                packet: Box::new(packet),
                metadata,
            };
        }

        if !combined.is_empty() {
            self.partials.insert(
                link.clone(),
                Partial {
                    bytes: combined,
                    updated_at_unix_millis: now_unix_millis,
                },
            );
        }

        InboundAppend::Waiting { metadata }
    }

    /// Discards every partial frame idle for longer than the stall window.
    ///
    /// [`Self::append`] can only reclaim memory for a peer that comes back. This is
    /// how memory is reclaimed from one that does not — the caller sweeps on its own
    /// maintenance tick. Without it the stall window is unenforceable in the exact
    /// case it was written for.
    pub fn reap_stalled(
        &mut self,
        limits: InboundLimits,
        now_unix_millis: u64,
    ) -> Vec<StalledPartial> {
        let stalled: Vec<StalledPartial> = self
            .partials
            .iter()
            .filter_map(|(link, partial)| {
                let idle = now_unix_millis.saturating_sub(partial.updated_at_unix_millis);
                if idle >= limits.stall_reset_ms {
                    Some(StalledPartial {
                        link: link.clone(),
                        discarded_bytes: partial.bytes.len(),
                        idle_ms: idle,
                    })
                } else {
                    None
                }
            })
            .collect();

        for entry in &stalled {
            self.partials.remove(&entry.link);
        }
        stalled
    }
}

// ---------------------------------------------------------------------------
// Log sampling
// ---------------------------------------------------------------------------

/// Rate-limits backpressure logging to the first event and every Nth after it.
///
/// Backpressure arrives in bursts of hundreds. Logging each one turns a recoverable
/// controller stall into an I/O storm of its own, which is why bitchat samples.
#[derive(Clone, Copy, Debug)]
pub struct BackpressureLogSampler {
    count: u64,
    interval: u64,
}

impl Default for BackpressureLogSampler {
    fn default() -> Self {
        Self::new(BACKPRESSURE_LOG_INTERVAL)
    }
}

impl BackpressureLogSampler {
    /// An interval of 0 is treated as 1 — every event logs — rather than being a
    /// division by zero.
    #[must_use]
    pub fn new(interval: u64) -> Self {
        Self {
            count: 0,
            interval: interval.max(1),
        }
    }

    /// Total events observed.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Records an event, returning its sequence number when it should be logged.
    pub fn record(&mut self) -> Option<u64> {
        self.count = self.count.saturating_add(1);
        if self.count == 1 || self.count % self.interval == 0 {
            Some(self.count)
        } else {
            None
        }
    }

    /// Restarts sampling, e.g. once the link recovers.
    pub fn reset(&mut self) {
        self.count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh::packet::{MessageType, SENDER_ID_LEN};

    fn link(name: &str) -> LinkId {
        LinkId::new(name)
    }

    fn packet(timestamp: u64) -> BitchatPacket {
        let mut sender = [0u8; SENDER_ID_LEN];
        for (i, slot) in sender.iter_mut().enumerate() {
            *slot = (i as u8) * 0x11;
        }
        BitchatPacket::new(
            MessageType::Message,
            sender,
            None,
            timestamp,
            vec![0xDE, 0xAD, 0xBE, 0xEF],
            3,
        )
    }

    fn frame(timestamp: u64) -> Vec<u8> {
        codec::encode(&packet(timestamp), false).expect("encode a well-formed packet")
    }

    // -- outbound write buffer ------------------------------------------------

    #[test]
    fn writes_leave_in_priority_order() {
        let mut buffer = OutboundWriteBuffer::new();
        let l = link("peer-1");

        let _ = buffer.enqueue(&l, vec![0x01; 4], WritePriority::FILE_TRANSFER, 64);
        let _ = buffer.enqueue(&l, vec![0x02; 4], WritePriority::HIGH, 64);
        let _ = buffer.enqueue(&l, vec![0x03; 4], WritePriority::fragment(2), 64);

        let taken = buffer.take_all(&l);
        let firsts: Vec<u8> = taken.iter().filter_map(|w| w.bytes.first().copied()).collect();
        assert_eq!(firsts, vec![0x02, 0x03, 0x01]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn equal_priorities_keep_fifo_order() {
        let mut buffer = OutboundWriteBuffer::new();
        let l = link("peer-1");
        for tag in 1u8..=4 {
            let _ = buffer.enqueue(&l, vec![tag; 2], WritePriority::fragment(8), 64);
        }
        let taken = buffer.take_all(&l);
        let firsts: Vec<u8> = taken.iter().filter_map(|w| w.bytes.first().copied()).collect();
        assert_eq!(firsts, vec![1, 2, 3, 4]);
    }

    #[test]
    fn smaller_transfers_outrank_larger_ones() {
        assert!(WritePriority::HIGH < WritePriority::fragment(2));
        assert!(WritePriority::fragment(2) < WritePriority::fragment(500));
        assert!(WritePriority::fragment(500) < WritePriority::FILE_TRANSFER);
        assert!(WritePriority::FILE_TRANSFER < WritePriority::LOW);
    }

    #[test]
    fn fragment_priority_clamps_absurd_totals() {
        assert_eq!(WritePriority::fragment(0), WritePriority::fragment(1));
        assert_eq!(
            WritePriority::fragment(usize::MAX),
            WritePriority::fragment(u16::MAX as usize)
        );
        // Even a clamped fragment must still outrank bulk traffic.
        assert!(WritePriority::fragment(usize::MAX) < WritePriority::FILE_TRANSFER);
    }

    #[test]
    fn byte_cap_admits_exactly_at_the_boundary_and_rejects_one_past_it() {
        let mut buffer = OutboundWriteBuffer::new();
        let l = link("peer-1");

        let at_cap = buffer.enqueue(&l, vec![0xAA; 16], WritePriority::HIGH, 16);
        assert_eq!(
            at_cap,
            WriteEnqueue::Enqueued {
                trimmed_bytes: 0,
                remaining_bytes: 16
            }
        );
        assert_eq!(buffer.queued_bytes(&l), 16);

        let mut fresh = OutboundWriteBuffer::new();
        let past_cap = fresh.enqueue(&l, vec![0xAA; 17], WritePriority::HIGH, 16);
        assert_eq!(past_cap, WriteEnqueue::Oversized { offered_bytes: 17 });
        // Nothing was allocated for the link at all.
        assert!(fresh.is_empty());
        assert_eq!(fresh.queued_bytes(&l), 0);
    }

    #[test]
    fn trimming_evicts_the_lowest_priority_tail_first() {
        let mut buffer = OutboundWriteBuffer::new();
        let l = link("peer-1");

        let _ = buffer.enqueue(&l, vec![0x01; 8], WritePriority::LOW, 16);
        let _ = buffer.enqueue(&l, vec![0x02; 8], WritePriority::FILE_TRANSFER, 16);
        let result = buffer.enqueue(&l, vec![0x03; 8], WritePriority::HIGH, 16);

        assert_eq!(
            result,
            WriteEnqueue::Enqueued {
                trimmed_bytes: 8,
                remaining_bytes: 16
            }
        );
        let taken = buffer.take_all(&l);
        let firsts: Vec<u8> = taken.iter().filter_map(|w| w.bytes.first().copied()).collect();
        assert_eq!(firsts, vec![0x03, 0x02]);
    }

    #[test]
    fn rejection_of_the_offered_write_is_reported_not_swallowed() {
        let mut buffer = OutboundWriteBuffer::new();
        let l = link("peer-1");
        let _ = buffer.enqueue(&l, vec![0x01; 8], WritePriority::HIGH, 8);

        // The queue is exactly full of higher-priority traffic, so this write sorts
        // to the tail and is trimmed straight back off. `Enqueued` alone would be a
        // lie here; `accepted` is what a courier commit reads.
        let admission =
            buffer.enqueue_reporting_acceptance(&l, vec![0x02; 8], WritePriority::LOW, 8);
        assert!(!admission.accepted);
        assert_eq!(
            admission.result,
            WriteEnqueue::Enqueued {
                trimmed_bytes: 8,
                remaining_bytes: 8
            }
        );

        let taken = buffer.take_all(&l);
        let firsts: Vec<u8> = taken.iter().filter_map(|w| w.bytes.first().copied()).collect();
        assert_eq!(firsts, vec![0x01]);
    }

    #[test]
    fn oversized_write_is_reported_as_not_accepted() {
        let mut buffer = OutboundWriteBuffer::new();
        let admission = buffer.enqueue_reporting_acceptance(
            &link("peer-1"),
            vec![0x01; 32],
            WritePriority::HIGH,
            16,
        );
        assert!(!admission.accepted);
        assert_eq!(
            admission.result,
            WriteEnqueue::Oversized { offered_bytes: 32 }
        );
    }

    #[test]
    fn a_surviving_write_reports_accepted() {
        let mut buffer = OutboundWriteBuffer::new();
        let l = link("peer-1");
        let _ = buffer.enqueue(&l, vec![0x01; 8], WritePriority::LOW, 8);

        let admission =
            buffer.enqueue_reporting_acceptance(&l, vec![0x02; 8], WritePriority::HIGH, 8);
        assert!(admission.accepted);
        let taken = buffer.take_all(&l);
        let firsts: Vec<u8> = taken.iter().filter_map(|w| w.bytes.first().copied()).collect();
        assert_eq!(firsts, vec![0x02]);
    }

    #[test]
    fn discard_all_removes_only_the_named_link() {
        let mut buffer = OutboundWriteBuffer::new();
        let gone = link("gone");
        let live = link("live");
        let _ = buffer.enqueue(&gone, vec![1], WritePriority::HIGH, 100);
        let _ = buffer.enqueue(&live, vec![2], WritePriority::HIGH, 100);

        buffer.discard_all(&gone);

        assert!(buffer.take_all(&gone).is_empty());
        let survivors: Vec<Vec<u8>> = buffer.take_all(&live).into_iter().map(|w| w.bytes).collect();
        assert_eq!(survivors, vec![vec![2u8]]);
    }

    #[test]
    fn prepend_restores_undrained_writes_ahead_of_newer_ones() {
        let mut buffer = OutboundWriteBuffer::new();
        let l = link("peer-1");
        let _ = buffer.enqueue(&l, vec![0x02], WritePriority::HIGH, 64);

        buffer.prepend(
            &l,
            vec![PendingWrite {
                priority: WritePriority::HIGH,
                bytes: vec![0x01],
            }],
        );

        let taken = buffer.take_all(&l);
        let firsts: Vec<u8> = taken.iter().filter_map(|w| w.bytes.first().copied()).collect();
        assert_eq!(firsts, vec![0x01, 0x02]);
    }

    #[test]
    fn prepend_of_nothing_leaves_the_queue_untouched() {
        let mut buffer = OutboundWriteBuffer::new();
        let l = link("peer-1");
        buffer.prepend(&l, Vec::new());
        assert!(buffer.is_empty());
    }

    #[test]
    fn one_link_cannot_grow_past_its_cap_under_sustained_backpressure() {
        let mut buffer = OutboundWriteBuffer::new();
        let l = link("greedy");
        let cap = 64 * 1024;

        // A peer that never drains: 10k writes offered, nothing taken.
        for i in 0..10_000u32 {
            let priority = if i % 3 == 0 {
                WritePriority::HIGH
            } else {
                WritePriority::LOW
            };
            let _ = buffer.enqueue(&l, vec![(i % 251) as u8; 1024], priority, cap);
            assert!(
                buffer.queued_bytes(&l) <= cap,
                "queue exceeded its cap at iteration {i}"
            );
        }

        assert_eq!(buffer.link_count(), 1);
        assert!(buffer.queued_bytes(&l) <= cap);
        assert_eq!(buffer.total_queued_bytes(), buffer.queued_bytes(&l));
    }

    #[test]
    fn rotating_link_identities_do_not_each_mint_a_fresh_cap() {
        let mut buffer = OutboundWriteBuffer::new();
        let cap = 4096;

        // BlueZ hands out a new object path per connection. Without discarding on
        // disconnect, 500 reconnects would hold 500 * cap bytes forever.
        for i in 0..500u32 {
            let l = link(&format!("/org/bluez/hci0/dev_{i}"));
            let _ = buffer.enqueue(&l, vec![0xAB; 4096], WritePriority::HIGH, cap);
            buffer.discard_all(&l);
        }

        assert!(buffer.is_empty());
        assert_eq!(buffer.total_queued_bytes(), 0);
    }

    #[test]
    fn clear_drops_every_link() {
        let mut buffer = OutboundWriteBuffer::new();
        let _ = buffer.enqueue(&link("a"), vec![1], WritePriority::HIGH, 64);
        let _ = buffer.enqueue(&link("b"), vec![2], WritePriority::HIGH, 64);
        assert_eq!(buffer.link_count(), 2);
        buffer.clear();
        assert!(buffer.is_empty());
    }

    #[test]
    fn link_ids_lists_only_links_holding_writes() {
        let mut buffer = OutboundWriteBuffer::new();
        let a = link("a");
        let _ = buffer.enqueue(&a, vec![1], WritePriority::HIGH, 64);
        let ids: Vec<&LinkId> = buffer.link_ids().collect();
        assert_eq!(ids, vec![&a]);

        let _ = buffer.take_all(&a);
        assert_eq!(buffer.link_ids().count(), 0);
    }

    // -- notification buffer --------------------------------------------------

    #[test]
    fn notifications_are_held_until_taken() {
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let result = buffer.enqueue(vec![0x01], Some(vec!["central-1".to_string()]), 2);
        assert!(result.accepted());
        assert_eq!(result.depth(), 1);

        let pending = buffer.take_all();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].bytes, vec![0x01]);
        assert_eq!(pending[0].targets, Some(vec!["central-1".to_string()]));
        assert!(buffer.is_empty());
    }

    #[test]
    fn count_cap_admits_exactly_at_the_boundary_and_rejects_one_past_it() {
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let cap = PENDING_NOTIFICATIONS_CAP_COUNT;

        for i in 0..cap {
            let result = buffer.enqueue(vec![i as u8], None, cap);
            assert!(result.accepted(), "packet {i} should fit under a cap of {cap}");
            assert_eq!(result.depth(), i + 1);
        }
        assert_eq!(buffer.len(), cap);
        assert!(buffer.is_full(cap));

        let overflow = buffer.enqueue(vec![0xFF], None, cap);
        assert!(!overflow.accepted());
        assert_eq!(overflow.depth(), cap);
        assert_eq!(buffer.len(), cap);
    }

    #[test]
    fn rejection_hands_the_payload_back_rather_than_swallowing_it() {
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let _ = buffer.enqueue(vec![0x01], None, 1);

        // A courier handoff commits on this result: it has already destroyed its own
        // copy of the envelope, so the envelope has to come back out.
        let envelope = vec![0xC0, 0xFF, 0xEE];
        let targets = Some(vec!["courier".to_string()]);
        match buffer.enqueue(envelope.clone(), targets.clone(), 1) {
            NotificationEnqueue::Full {
                depth,
                bytes,
                targets: returned,
            } => {
                assert_eq!(depth, 1);
                assert_eq!(bytes, envelope);
                assert_eq!(returned, targets);
            }
            NotificationEnqueue::Enqueued { .. } => panic!("expected the cap to reject"),
        }
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn a_zero_cap_rejects_everything() {
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let result = buffer.enqueue(vec![0x01], None, 0);
        assert!(!result.accepted());
        assert_eq!(result.depth(), 0);
        assert!(buffer.is_empty());
    }

    #[test]
    fn prepend_restores_unsent_notifications_ahead_of_newer_items() {
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let _ = buffer.enqueue(vec![0x02], Some(vec!["new".to_string()]), 4);
        buffer.prepend(vec![PendingNotification {
            bytes: vec![0x01],
            targets: Some(vec!["old".to_string()]),
        }]);

        let pending = buffer.take_all();
        let firsts: Vec<u8> = pending.iter().filter_map(|n| n.bytes.first().copied()).collect();
        assert_eq!(firsts, vec![0x01, 0x02]);
    }

    #[test]
    fn remove_target_drops_only_that_subscribers_ciphertext() {
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let _ = buffer.enqueue(vec![1], Some(vec!["gone".to_string()]), 4);
        let _ = buffer.enqueue(
            vec![2],
            Some(vec!["gone".to_string(), "live".to_string()]),
            4,
        );
        let _ = buffer.enqueue(vec![3], None, 4);

        buffer.remove_target(|t| t == "gone");
        let remaining = buffer.take_all();

        let payloads: Vec<Vec<u8>> = remaining.iter().map(|n| n.bytes.clone()).collect();
        assert_eq!(payloads, vec![vec![2u8], vec![3u8]]);
        assert_eq!(remaining[0].targets, Some(vec!["live".to_string()]));
        assert_eq!(remaining[1].targets, None);
    }

    #[test]
    fn clear_drops_buffered_notifications() {
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let _ = buffer.enqueue(vec![0x01], None, 4);
        buffer.clear();
        assert!(buffer.is_empty());
    }

    #[test]
    fn notification_count_stays_capped_under_sustained_backpressure() {
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let cap = PENDING_NOTIFICATIONS_CAP_COUNT;
        let mut rejected = 0usize;

        for i in 0..10_000u32 {
            if !buffer.enqueue(vec![0u8; 512], None, cap).accepted() {
                rejected += 1;
            }
            assert!(buffer.len() <= cap, "depth exceeded the cap at iteration {i}");
        }

        assert_eq!(buffer.len(), cap);
        assert_eq!(rejected, 10_000 - cap);
    }

    // -- bounded retry --------------------------------------------------------

    #[test]
    fn backoff_grows_linearly_and_never_schedules_for_now() {
        let policy = NotificationRetryPolicy::default();
        assert_eq!(policy.backoff_ms(0), NOTIFICATION_RETRY_DELAY_MS);
        assert_eq!(policy.backoff_ms(1), NOTIFICATION_RETRY_DELAY_MS);
        assert_eq!(policy.backoff_ms(2), NOTIFICATION_RETRY_DELAY_MS * 2);
        assert_eq!(policy.backoff_ms(80), NOTIFICATION_RETRY_DELAY_MS * 80);
    }

    #[test]
    fn a_retry_polled_before_its_due_time_consumes_no_attempt() {
        let policy = NotificationRetryPolicy::default();
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let _ = buffer.enqueue(vec![0x01], None, 1);

        let retry = NotificationRetry::new(vec![0xAA], None, 1_000);
        let retry = match retry.poll(&mut buffer, &policy, 1, 1_000) {
            RetryStep::Waiting(r) => r,
            other => panic!("expected a wait, got {other:?}"),
        };
        assert_eq!(retry.attempt(), 1);
        assert_eq!(retry.due_at_unix_millis(), 1_025);
        assert!(!retry.is_due(1_024));

        // Polling early must not burn the budget — otherwise a caller without a
        // timer exhausts 80 attempts in microseconds.
        let mut retry = retry;
        for _ in 0..1_000 {
            retry = match retry.poll(&mut buffer, &policy, 1, 1_000) {
                RetryStep::Waiting(r) => r,
                other => panic!("expected a wait, got {other:?}"),
            };
            assert_eq!(retry.attempt(), 1);
        }
        assert!(retry.is_due(1_025));
    }

    #[test]
    fn retry_stops_at_the_maximum_and_drops_loudly() {
        let policy = NotificationRetryPolicy::default();
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let _ = buffer.enqueue(vec![0x01], None, 1);

        let envelope = vec![0xC0, 0xFF, 0xEE];
        let targets = Some(vec!["central-1".to_string()]);
        let mut retry = NotificationRetry::new(envelope.clone(), targets.clone(), 0);
        let mut now = 0u64;
        let mut polls = 0u32;

        let dropped = loop {
            polls += 1;
            assert!(polls < 1_000, "retry never terminated");
            match retry.poll(&mut buffer, &policy, 1, now) {
                RetryStep::Waiting(next) => {
                    now = next.due_at_unix_millis();
                    retry = next;
                }
                RetryStep::Admitted { .. } => panic!("the queue was full throughout"),
                step @ RetryStep::Dropped { .. } => break step,
            }
        };

        // Attempt 0 through 79 reschedule; the 81st poll is at attempt 80, which is
        // the ceiling, so it drops. One admission try per attempt, plus the final one.
        assert_eq!(polls, NOTIFICATION_RETRY_MAX_ATTEMPTS + 1);
        match dropped {
            RetryStep::Dropped {
                bytes,
                targets: returned,
                attempts,
                depth,
            } => {
                assert_eq!(attempts, NOTIFICATION_RETRY_MAX_ATTEMPTS);
                assert_eq!(depth, 1);
                // Loud, not silent: the payload comes back so the drop can be logged
                // with what was actually lost.
                assert_eq!(bytes, envelope);
                assert_eq!(returned, targets);
            }
            other => panic!("expected a drop, got {other:?}"),
        }

        // Total elapsed is the linear sum 25 * (1 + 2 + ... + 80) ms.
        let expected: u64 = (1..=u64::from(NOTIFICATION_RETRY_MAX_ATTEMPTS))
            .map(|a| a * NOTIFICATION_RETRY_DELAY_MS)
            .sum();
        assert_eq!(now, expected);
    }

    #[test]
    fn a_retry_is_admitted_once_the_queue_drains() {
        let policy = NotificationRetryPolicy::default();
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let _ = buffer.enqueue(vec![0x01], None, 1);

        let retry = NotificationRetry::new(vec![0xAA], None, 0);
        let retry = match retry.poll(&mut buffer, &policy, 1, 0) {
            RetryStep::Waiting(r) => r,
            other => panic!("expected a wait, got {other:?}"),
        };

        let drained = buffer.take_all();
        assert_eq!(drained.len(), 1);

        let due = retry.due_at_unix_millis();
        match retry.poll(&mut buffer, &policy, 1, due) {
            RetryStep::Admitted { depth, attempts } => {
                assert_eq!(depth, 1);
                assert_eq!(attempts, 2);
            }
            other => panic!("expected admission, got {other:?}"),
        }
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn a_first_poll_into_a_free_queue_admits_immediately() {
        let policy = NotificationRetryPolicy::default();
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let retry = NotificationRetry::new(vec![0xAA], None, 500);
        assert_eq!(retry.bytes(), &[0xAA]);
        assert!(retry.targets().is_none());

        match retry.poll(&mut buffer, &policy, 4, 500) {
            RetryStep::Admitted { depth, attempts } => {
                assert_eq!(depth, 1);
                assert_eq!(attempts, 1);
            }
            other => panic!("expected admission, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_attempt_policy_drops_on_the_first_refusal() {
        let policy = NotificationRetryPolicy {
            delay_ms: 25,
            max_attempts: 0,
        };
        let mut buffer = OutboundNotificationBuffer::<String>::new();
        let _ = buffer.enqueue(vec![0x01], None, 1);

        match NotificationRetry::new(vec![0xAA], None, 0).poll(&mut buffer, &policy, 1, 0) {
            RetryStep::Dropped { attempts, bytes, .. } => {
                assert_eq!(attempts, 0);
                assert_eq!(bytes, vec![0xAA]);
            }
            other => panic!("expected an immediate drop, got {other:?}"),
        }
    }

    // -- inbound assembly -----------------------------------------------------

    #[test]
    fn chunked_writes_reassemble_into_a_packet() {
        let mut assembler = InboundAssembler::new();
        let limits = InboundLimits {
            cap_bytes: 1024,
            stall_reset_ms: 250,
        };
        let l = link("central-1");
        let bytes = frame(0x0102030405);
        let split = (bytes.len() / 2).max(1);

        let first = assembler.append(
            &l,
            &[InboundChunk {
                offset: 0,
                bytes: bytes[..split].to_vec(),
            }],
            limits,
            0,
        );
        match first {
            InboundAppend::Waiting { metadata } => {
                assert_eq!(metadata.accumulated_bytes, split);
                assert_eq!(metadata.appended_bytes, split);
                assert_eq!(metadata.offsets, vec![0]);
                assert_eq!(metadata.packet_type, Some(MessageType::Message.as_u8()));
                assert_eq!(metadata.stall_discarded_bytes, 0);
            }
            other => panic!("expected a partial frame to wait, got {other:?}"),
        }
        assert_eq!(assembler.buffered_bytes(&l), split);

        let second = assembler.append(
            &l,
            &[InboundChunk {
                offset: split,
                bytes: bytes[split..].to_vec(),
            }],
            limits,
            10,
        );
        match second {
            InboundAppend::Decoded { packet, metadata } => {
                assert_eq!(packet.timestamp, 0x0102030405);
                assert_eq!(packet.payload, vec![0xDE, 0xAD, 0xBE, 0xEF]);
                assert_eq!(metadata.accumulated_bytes, bytes.len());
                assert_eq!(metadata.offsets, vec![split]);
            }
            other => panic!("expected the completed frame to decode, got {other:?}"),
        }
        // Decoding releases the buffer.
        assert!(assembler.is_empty());
    }

    #[test]
    fn multiple_offset_chunks_merge_in_one_call() {
        let mut assembler = InboundAssembler::new();
        let limits = InboundLimits {
            cap_bytes: 1024,
            stall_reset_ms: 250,
        };
        let bytes = frame(0xABC);
        let split = (bytes.len() / 3).max(1);

        // Out of order on purpose: GATT prepared writes carry offsets precisely
        // because they are not guaranteed to arrive in sequence.
        let result = assembler.append(
            &link("central-1"),
            &[
                InboundChunk {
                    offset: split,
                    bytes: bytes[split..].to_vec(),
                },
                InboundChunk {
                    offset: 0,
                    bytes: bytes[..split].to_vec(),
                },
            ],
            limits,
            0,
        );

        match result {
            InboundAppend::Decoded { packet, metadata } => {
                assert_eq!(packet.timestamp, 0xABC);
                assert_eq!(metadata.appended_bytes, bytes.len());
                assert_eq!(metadata.offsets, vec![split, 0]);
            }
            other => panic!("expected merged chunks to decode, got {other:?}"),
        }
    }

    #[test]
    fn inbound_cap_admits_exactly_at_the_boundary_and_rejects_one_past_it() {
        let mut assembler = InboundAssembler::new();
        let l = link("central-1");
        let limits = InboundLimits {
            cap_bytes: 8,
            stall_reset_ms: 250,
        };

        let at_cap = assembler.append(
            &l,
            &[InboundChunk {
                offset: 0,
                bytes: vec![0xAA; 8],
            }],
            limits,
            0,
        );
        assert!(matches!(at_cap, InboundAppend::Waiting { .. }));
        assert_eq!(assembler.buffered_bytes(&l), 8);

        let past_cap = assembler.append(
            &l,
            &[InboundChunk {
                offset: 8,
                bytes: vec![0xBB; 1],
            }],
            limits,
            1,
        );
        match past_cap {
            InboundAppend::Oversized { metadata } => {
                assert_eq!(metadata.accumulated_bytes, 9);
                assert_eq!(metadata.appended_bytes, 1);
            }
            other => panic!("expected the cap to reject, got {other:?}"),
        }
        // The partial frame is released rather than pinned at the cap.
        assert!(assembler.is_empty());
    }

    #[test]
    fn an_oversized_write_is_dropped_and_a_later_clean_frame_still_decodes() {
        let mut assembler = InboundAssembler::new();
        let l = link("central-1");

        let oversized = assembler.append(
            &l,
            &[InboundChunk {
                offset: 0,
                bytes: vec![0xAA; 8],
            }],
            InboundLimits {
                cap_bytes: 4,
                stall_reset_ms: 250,
            },
            0,
        );
        match oversized {
            InboundAppend::Oversized { metadata } => {
                assert_eq!(metadata.accumulated_bytes, 8);
                assert_eq!(metadata.appended_bytes, 8);
            }
            other => panic!("expected an oversized rejection, got {other:?}"),
        }

        let bytes = frame(0xDEF);
        let decoded = assembler.append(
            &l,
            &[InboundChunk {
                offset: 0,
                bytes: bytes.clone(),
            }],
            InboundLimits {
                cap_bytes: 1024,
                stall_reset_ms: 250,
            },
            1,
        );
        match decoded {
            InboundAppend::Decoded { packet, .. } => assert_eq!(packet.timestamp, 0xDEF),
            other => panic!("expected the clean frame to decode, got {other:?}"),
        }
    }

    #[test]
    fn a_hostile_offset_cannot_force_an_allocation() {
        let mut assembler = InboundAssembler::new();
        let l = link("hostile");
        let limits = InboundLimits::default();

        // bitchat zero-fills to `offset + len` and only then checks the cap, so a
        // 4-byte write at this offset would ask for terabytes. The bound is checked
        // before anything is resized.
        let huge = assembler.append(
            &l,
            &[InboundChunk {
                offset: 1 << 40,
                bytes: vec![0xAA; 4],
            }],
            limits,
            0,
        );
        assert!(matches!(huge, InboundAppend::Oversized { .. }));
        assert!(assembler.is_empty());

        // And an offset that would overflow the address space at all.
        let overflowing = assembler.append(
            &l,
            &[InboundChunk {
                offset: usize::MAX,
                bytes: vec![0xAA; 4],
            }],
            limits,
            0,
        );
        assert!(matches!(overflowing, InboundAppend::Oversized { .. }));
        assert!(assembler.is_empty());
        assert_eq!(assembler.total_buffered_bytes(), 0);
    }

    #[test]
    fn a_gap_between_chunks_is_zero_filled_and_simply_never_parses() {
        let mut assembler = InboundAssembler::new();
        let l = link("central-1");
        let limits = InboundLimits {
            cap_bytes: 1024,
            stall_reset_ms: 250,
        };

        let result = assembler.append(
            &l,
            &[InboundChunk {
                offset: 64,
                bytes: vec![0xAA; 4],
            }],
            limits,
            0,
        );
        match result {
            InboundAppend::Waiting { metadata } => {
                assert_eq!(metadata.accumulated_bytes, 68);
                assert_eq!(metadata.appended_bytes, 4);
                assert_eq!(metadata.packet_type, Some(0));
            }
            other => panic!("expected a hole to keep waiting, got {other:?}"),
        }
        assert_eq!(assembler.buffered_bytes(&l), 68);
    }

    #[test]
    fn a_partial_frame_is_reset_after_the_stall_window() {
        let mut assembler = InboundAssembler::new();
        let l = link("central-1");
        let limits = InboundLimits {
            cap_bytes: 1024,
            stall_reset_ms: 250,
        };
        let bytes = frame(0x11);
        let split = (bytes.len() / 2).max(1);

        let _ = assembler.append(
            &l,
            &[InboundChunk {
                offset: 0,
                bytes: bytes[..split].to_vec(),
            }],
            limits,
            1_000,
        );
        assert_eq!(assembler.buffered_bytes(&l), split);

        // One millisecond short of the window: the old bytes are still there, so the
        // completing tail decodes.
        let mut early = assembler.clone();
        let completed = early.append(
            &l,
            &[InboundChunk {
                offset: split,
                bytes: bytes[split..].to_vec(),
            }],
            limits,
            1_000 + limits.stall_reset_ms - 1,
        );
        match completed {
            InboundAppend::Decoded { metadata, .. } => {
                assert_eq!(metadata.stall_discarded_bytes, 0);
            }
            other => panic!("expected the frame to still complete, got {other:?}"),
        }

        // Exactly at the window: the stalled prefix is discarded first, so the same
        // tail lands on an empty buffer and cannot decode.
        let late = assembler.append(
            &l,
            &[InboundChunk {
                offset: split,
                bytes: bytes[split..].to_vec(),
            }],
            limits,
            1_000 + limits.stall_reset_ms,
        );
        match late {
            InboundAppend::Waiting { metadata } => {
                assert_eq!(metadata.stall_discarded_bytes, split);
                // Reassembly restarted: only the tail, written at its own offset.
                assert_eq!(metadata.accumulated_bytes, bytes.len());
            }
            other => panic!("expected the stalled prefix to be discarded, got {other:?}"),
        }
    }

    #[test]
    fn reap_stalled_reclaims_partials_from_peers_that_never_come_back() {
        let mut assembler = InboundAssembler::new();
        let limits = InboundLimits {
            cap_bytes: 1024,
            stall_reset_ms: 250,
        };

        // Three peers open a frame and go silent; one keeps writing.
        for i in 0..3 {
            let _ = assembler.append(
                &link(&format!("silent-{i}")),
                &[InboundChunk {
                    offset: 0,
                    bytes: vec![0xAA; 100],
                }],
                limits,
                1_000,
            );
        }
        let chatty = link("chatty");
        let _ = assembler.append(
            &chatty,
            &[InboundChunk {
                offset: 0,
                bytes: vec![0xBB; 100],
            }],
            limits,
            1_200,
        );

        assert_eq!(assembler.link_count(), 4);
        assert_eq!(assembler.total_buffered_bytes(), 400);

        let reaped = assembler.reap_stalled(limits, 1_300);
        assert_eq!(reaped.len(), 3);
        for entry in &reaped {
            assert_eq!(entry.discarded_bytes, 100);
            assert_eq!(entry.idle_ms, 300);
            assert!(entry.link.as_str().starts_with("silent-"));
        }
        assert_eq!(assembler.link_count(), 1);
        assert_eq!(assembler.buffered_bytes(&chatty), 100);

        // Nothing to reclaim twice.
        assert!(assembler.reap_stalled(limits, 1_300).is_empty());
    }

    #[test]
    fn discard_and_clear_release_partial_writes() {
        let mut assembler = InboundAssembler::new();
        let limits = InboundLimits {
            cap_bytes: 1024,
            stall_reset_ms: 250,
        };
        let a = link("a");
        let b = link("b");
        let bytes = frame(0x22);
        let split = (bytes.len() / 2).max(1);

        for l in [&a, &b] {
            let _ = assembler.append(
                l,
                &[InboundChunk {
                    offset: 0,
                    bytes: bytes[..split].to_vec(),
                }],
                limits,
                0,
            );
        }

        assert_eq!(assembler.discard(&a), split);
        assert_eq!(assembler.link_count(), 1);

        // The earlier prefix is gone, so the tail alone cannot decode.
        let result = assembler.append(
            &a,
            &[InboundChunk {
                offset: split,
                bytes: bytes[split..].to_vec(),
            }],
            limits,
            1,
        );
        assert!(matches!(result, InboundAppend::Waiting { .. }));

        assembler.clear();
        assert!(assembler.is_empty());
        assert_eq!(assembler.total_buffered_bytes(), 0);
    }

    #[test]
    fn empty_chunks_are_ignored_and_leave_no_entry() {
        let mut assembler = InboundAssembler::new();
        let l = link("central-1");
        let result = assembler.append(
            &l,
            &[InboundChunk {
                offset: 0,
                bytes: Vec::new(),
            }],
            InboundLimits::default(),
            0,
        );
        match result {
            InboundAppend::Waiting { metadata } => {
                assert_eq!(metadata.appended_bytes, 0);
                assert!(metadata.offsets.is_empty());
                assert_eq!(metadata.packet_type, None);
            }
            other => panic!("expected an empty write to be a no-op, got {other:?}"),
        }
        assert!(assembler.is_empty());
    }

    #[test]
    fn inbound_memory_stays_bounded_under_sustained_partial_writes() {
        let mut assembler = InboundAssembler::new();
        let limits = InboundLimits {
            cap_bytes: 4096,
            stall_reset_ms: 250,
        };

        // 200 peers, each dribbling partial frames forever and never completing one.
        // Per-link memory is bounded by the cap; the link count is bounded by the
        // connection budget, and reaping bounds it further.
        let mut now = 0u64;
        for round in 0..50u64 {
            for peer in 0..200u32 {
                let _ = assembler.append(
                    &link(&format!("peer-{peer}")),
                    &[InboundChunk {
                        offset: (round as usize * 64) % 2048,
                        bytes: vec![0xAA; 64],
                    }],
                    limits,
                    now,
                );
            }
            now += 10;
            assert!(
                assembler.total_buffered_bytes() <= 200 * limits.cap_bytes,
                "inbound memory grew past 200 capped links in round {round}"
            );
        }

        assert_eq!(assembler.link_count(), 200);
        for peer in 0..200u32 {
            assert!(assembler.buffered_bytes(&link(&format!("peer-{peer}"))) <= limits.cap_bytes);
        }

        // Everyone goes quiet: the sweep gives all of it back.
        let reaped = assembler.reap_stalled(limits, now + limits.stall_reset_ms);
        assert_eq!(reaped.len(), 200);
        assert_eq!(assembler.total_buffered_bytes(), 0);
        assert!(assembler.is_empty());
    }

    // -- log sampling ---------------------------------------------------------

    #[test]
    fn the_sampler_logs_the_first_event_and_every_interval_after() {
        let mut sampler = BackpressureLogSampler::new(BACKPRESSURE_LOG_INTERVAL);
        let logged: Vec<u64> = (0..60).filter_map(|_| sampler.record()).collect();
        assert_eq!(logged, vec![1, 25, 50]);
        assert_eq!(sampler.count(), 60);

        sampler.reset();
        assert_eq!(sampler.count(), 0);
        assert_eq!(sampler.record(), Some(1));
    }

    #[test]
    fn a_zero_interval_sampler_logs_everything_rather_than_dividing_by_zero() {
        let mut sampler = BackpressureLogSampler::new(0);
        assert_eq!(sampler.record(), Some(1));
        assert_eq!(sampler.record(), Some(2));
        assert_eq!(sampler.record(), Some(3));
    }

    // -- error classification -------------------------------------------------

    #[test]
    fn only_a_rejected_write_is_worth_buffering() {
        assert!(should_buffer(&LinkError::WriteRejected("link-1".into())));
        assert!(!should_buffer(&LinkError::LinkGone("link-1".into())));
        assert!(!should_buffer(&LinkError::PayloadTooLarge {
            actual: 600,
            max: 512
        }));
        assert!(!should_buffer(&LinkError::AdapterUnavailable("off".into())));
        assert!(!should_buffer(&LinkError::Backend("dbus".into())));
    }
}
