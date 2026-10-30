//! Write backpressure buffering — outbound writes and inbound reassembly.
//!
//! BLE controllers refuse writes as a matter of course. `canSendWriteWithoutResponse`
//! goes false, `updateValue` returns false, and there is no retransmission anywhere
//! below us: a rejected write that the caller discards is a packet that simply never
//! existed. So rejection has to mean *queue and retry*, which immediately raises the
//! question this module exists to answer — what stops the queue from eating the
//! process when the peer on the other end just stops reading?
//!
//! Two answers, one per direction of pressure:
//!
//! - [`OutboundWriteBuffer`] — per-link, **byte**-capped, priority-ordered. Over the
//!   cap it trims from the low-priority tail, so a stalled file transfer cannot
//!   starve a handshake. Keyed by [`crate::link::LinkId`] and dropped wholesale on disconnect,
//!   because rotating link identities would otherwise each mint a fresh cap.
//! - [`InboundAssembler`] — per-link reassembly of offset-addressed writes, with a
//!   hard byte cap *and* a stall window, because a peer that opens a large write and
//!   goes quiet pins that memory until something reclaims it.
//!
//! Sans-IO throughout: no clocks, no timers, no async. Time enters as a
//! `now_unix_millis` parameter.
//!
//! [`OutboundWriteBuffer`] reports with [`WriteAdmission::accepted`], matching
//! `writeOrEnqueueIfAccepted`: trimming can evict items *other* than the one just
//! offered, so no single returned payload could describe the outcome honestly. A
//! caller with durable upstream state keeps it retryable on `accepted == false`.
//!
//! Ported from `BLEOutboundWriteBuffer.swift`, `BLEInboundWriteBuffer.swift`, and
//! the stall window from `NotificationStreamAssembler.swift`.

use crate::error::LinkError;

// ---------------------------------------------------------------------------
// Constants — bitchat `Services/TransportConfig.swift`
// ---------------------------------------------------------------------------

/// Per-link ceiling on queued outbound write bytes.
///
/// One megabyte per link, not process-wide: the cap is what a *single* misbehaving
/// peer can cost us, and links are already bounded by the connection budget.
pub const PENDING_WRITE_BUFFER_CAP_BYTES: usize = 1_000_000;

/// Hard ceiling on one link's partially assembled inbound frame.
pub const NOTIFICATION_ASSEMBLER_HARD_CAP_BYTES: usize = 8 * 1024 * 1024;

/// How long a partial frame may sit without progress before it is discarded.
pub const ASSEMBLER_STALL_RESET_MS: u64 = 250;

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

mod inbound;
mod outbound;

pub use inbound::{
    InboundAppend, InboundAppendMetadata, InboundAssembler, InboundChunk, InboundLimits,
    StalledPartial,
};
pub use outbound::{
    OutboundWriteBuffer, PendingWrite, WriteAdmission, WriteEnqueue, WritePriority,
};

#[cfg(test)]
mod tests;
