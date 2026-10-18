use std::collections::BTreeMap;

use chatter_mesh::BitchatPacket;
use chatter_mesh::codec;

use super::{ASSEMBLER_STALL_RESET_MS, NOTIFICATION_ASSEMBLER_HARD_CAP_BYTES};
use crate::link::LinkId;

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
