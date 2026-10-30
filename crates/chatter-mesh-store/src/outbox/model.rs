use super::*;

/// A private message being retained until it is acknowledged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedMessage {
    /// Plaintext, in memory only. Sealed before it reaches [`Storage`].
    pub content: String,
    /// The recipient's nickname at send time, needed to re-address the resend.
    pub nickname: String,
    pub message_id: String,
    /// Unix seconds. The TTL and the recovery sort are both against this.
    pub timestamp: u64,
    /// Sends made against a weak reachability signal. Only those count — see
    /// [`LinkState::Insecure`].
    pub send_attempts: u32,
    /// Noise static keys of couriers already carrying this message, so a deposit
    /// retry adds couriers instead of re-burning the ones already spent.
    pub deposited_courier_keys: BTreeSet<[u8; 32]>,
}

impl QueuedMessage {
    /// A fresh message with no attempts and no couriers.
    pub fn new(
        content: impl Into<String>,
        nickname: impl Into<String>,
        message_id: impl Into<String>,
        timestamp: u64,
    ) -> Self {
        Self {
            content: content.into(),
            nickname: nickname.into(),
            message_id: message_id.into(),
            timestamp,
            send_attempts: 0,
            deposited_courier_keys: BTreeSet::new(),
        }
    }

    /// Builder form for the router's "sent once already" case.
    #[must_use]
    pub fn with_send_attempts(mut self, attempts: u32) -> Self {
        self.send_attempts = attempts;
        self
    }

    /// Whether this message may ride with another courier.
    pub fn remaining_courier_slots(&self) -> usize {
        MAX_COURIERS_PER_MESSAGE.saturating_sub(self.deposited_courier_keys.len())
    }

    /// Whether the courier holding this static key already carries the message.
    pub fn is_carried_by(&self, courier_key: &[u8; 32]) -> bool {
        self.deposited_courier_keys.contains(courier_key)
    }

    /// Whether the retention window has closed on this message.
    pub fn is_expired(&self, now_unix_secs: u64) -> bool {
        // A clock that went backwards leaves the message un-expired rather than
        // instantly expiring every queue, which matches Swift's signed interval.
        now_unix_secs.saturating_sub(self.timestamp) > MESSAGE_TTL_SECS
    }
}

/// How good the route to a peer is *right now*, as judged by the router.
///
/// The four cases are the four branches of bitchat's `flushOutbox`, and the
/// difference between the middle two is the subtlest rule in the whole store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// Connected, with a session that can complete an encrypted delivery now.
    /// Strong enough to stop retaining.
    Secure,
    /// Connected, but no secure session yet.
    ///
    /// The link binding is forgeable — a replayed announce can bind an absent
    /// peer's ID to the replayer's link — so the copy is retained. But the send
    /// **does not** count toward the attempt cap: the message really did go out
    /// over a live link, and a peer whose handshake stalls across reconnect
    /// flapping would otherwise burn all [`MAX_SEND_ATTEMPTS`] attempts and lose
    /// the very copy this retention exists to preserve. Retention stays bounded by
    /// the TTL and the per-peer FIFO cap instead.
    Insecure,
    /// Not connected, but a freshness heuristic says the peer is around. The send
    /// can silently go nowhere, so it is retained *and* charged an attempt —
    /// otherwise a peer on a client that never acks is retried forever.
    Reachable,
    /// No route. Nothing is sent, nothing is charged.
    Unreachable,
}

/// Why a retained message was given up on.
///
/// Every one of these has to reach the UI: a message that leaves the outbox without
/// an ack and without a visible failure sits on "sending" forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// Past the 24 h retention window.
    Expired,
    /// Sent [`MAX_SEND_ATTEMPTS`] times against a weak signal with no ack.
    AttemptsExhausted,
    /// Evicted by the per-peer FIFO cap to make room for newer mail.
    QueueOverflow,
}

/// A message that left the outbox undelivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DroppedMessage {
    pub message_id: String,
    pub peer: PeerId,
    pub reason: DropReason,
}

/// What a flush decided.
///
/// The store has already applied the state changes by the time this is returned:
/// `send` messages under [`LinkState::Secure`] are gone from the outbox, attempts
/// under [`LinkState::Reachable`] are already charged, and `dropped` messages are
/// already removed. The router transmits `send` and surfaces `dropped`.
///
/// Acceptance by a transport is deliberately not fed back: bitchat treats the send
/// as made either way, and only an ack clears a retained copy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlushPlan {
    /// Transmit these, oldest first.
    pub send: Vec<QueuedMessage>,
    /// Report these as failed.
    pub dropped: Vec<DroppedMessage>,
}

impl FlushPlan {
    pub fn is_empty(&self) -> bool {
        self.send.is_empty() && self.dropped.is_empty()
    }
}

/// Which kind of acknowledgment cleared a message.
///
/// Ordered so that `max` is the stronger one: a read ack implies delivery, and must
/// never be downgraded by a delivery ack that arrives afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AckKind {
    Delivered,
    Read,
}

/// The result of acking a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckOutcome {
    /// Whether a retained copy was actually removed. False when the ack arrived for
    /// a message this view never had — a duplicate ack, or one that landed while
    /// the durable queue was still unreadable. Either way the ack is remembered.
    pub cleared: bool,
    /// The strongest ack seen for this message ID, which is what the UI should
    /// display. A [`AckKind::Delivered`] arriving after a [`AckKind::Read`] reports
    /// `Read` here.
    pub strongest: AckKind,
}
