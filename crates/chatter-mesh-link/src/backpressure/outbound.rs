use std::collections::BTreeMap;

use crate::link::LinkId;

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
    ///
    /// Returns the packet type byte of everything thrown away, so the caller can
    /// say whether the loss mattered. An announce is re-sent on the next interval
    /// and a fragment is not: a queue full of `Fragment` is a transfer that has
    /// just lost its middle, which no other line records.
    pub fn discard_all(&mut self, link: &LinkId) -> Vec<u8> {
        self.writes
            .remove(link)
            .map(|queue| {
                queue
                    .iter()
                    .filter_map(|write| write.bytes.get(1).copied())
                    .collect()
            })
            .unwrap_or_default()
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
