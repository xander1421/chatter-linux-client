use super::*;

impl OutboxStore {
    /// Retain a message for a peer.
    ///
    /// Re-enqueueing an ID that is already queued replaces the entry — keeping the
    /// attempt count fresh and moving it to the back — but **unions the courier
    /// keys**, or the replacement re-burns slots already spent on it.
    ///
    /// Returns the message evicted by the per-peer FIFO cap, if the queue was full.
    #[must_use = "an evicted message must be surfaced or it stays on 'sending' forever"]
    pub fn enqueue(&mut self, peer: &PeerId, message: QueuedMessage) -> Option<DroppedMessage> {
        let mut message = message;
        let queue = self.outbox.entry(peer.clone()).or_default();

        if let Some(index) = queue
            .iter()
            .position(|m| m.message_id == message.message_id)
        {
            if let Some(existing) = queue.get(index) {
                message
                    .deposited_courier_keys
                    .extend(existing.deposited_courier_keys.iter().copied());
            }
            queue.remove(index);
        }
        queue.push(message);

        let evicted = if queue.len() > MAX_MESSAGES_PER_PEER {
            let evicted = queue.remove(0);
            Some(DroppedMessage {
                message_id: evicted.message_id,
                peer: peer.clone(),
                reason: DropReason::QueueOverflow,
            })
        } else {
            None
        };

        self.dirty = true;
        evicted
    }

    /// Record that couriers now carry a message.
    ///
    /// Unions rather than replaces, and silently does nothing when the message has
    /// already been acked away — a deposit confirmation can land after delivery.
    pub fn record_courier_deposit(
        &mut self,
        peer: &PeerId,
        message_id: &str,
        courier_keys: &[[u8; 32]],
    ) {
        let Some(queue) = self.outbox.get_mut(peer) else {
            return;
        };
        let Some(message) = queue.iter_mut().find(|m| m.message_id == message_id) else {
            return;
        };
        let before = message.deposited_courier_keys.len();
        message
            .deposited_courier_keys
            .extend(courier_keys.iter().copied());
        if message.deposited_courier_keys.len() != before {
            self.dirty = true;
        }
    }
    /// Decide what to do with everything retained for one peer.
    ///
    /// Expired mail is dropped first, whatever the link is. Then, per
    /// [`LinkState`]: a secure link keeps the copy until an ACK, an insecure one keeps it
    /// without charging an attempt, a merely reachable one keeps it and charges
    /// one, and no route at all leaves it untouched.
    pub fn flush(&mut self, peer: &PeerId, link: LinkState, now_unix_secs: u64) -> FlushPlan {
        let mut plan = FlushPlan::default();
        let Some(queue) = self.outbox.remove(peer) else {
            return plan;
        };
        let mut remaining: Vec<QueuedMessage> = Vec::new();

        for mut message in queue {
            if message.is_expired(now_unix_secs) {
                plan.dropped.push(DroppedMessage {
                    message_id: message.message_id,
                    peer: peer.clone(),
                    reason: DropReason::Expired,
                });
                continue;
            }

            match link {
                LinkState::Secure => {
                    // A secure session does not prove that the queued radio write
                    // survived a disconnect. Only the recipient's ACK does.
                    plan.send.push(message.clone());
                    remaining.push(message);
                }
                LinkState::Insecure => {
                    // Sent over a live link, so no attempt is charged. See the
                    // variant's documentation — this is the rule that keeps a
                    // flapping handshake from destroying the retained copy.
                    plan.send.push(message.clone());
                    remaining.push(message);
                }
                LinkState::Reachable => {
                    if message.send_attempts >= MAX_SEND_ATTEMPTS {
                        plan.dropped.push(DroppedMessage {
                            message_id: message.message_id,
                            peer: peer.clone(),
                            reason: DropReason::AttemptsExhausted,
                        });
                        continue;
                    }
                    plan.send.push(message.clone());
                    message.send_attempts = message.send_attempts.saturating_add(1);
                    remaining.push(message);
                }
                LinkState::Unreachable => remaining.push(message),
            }
        }

        if !remaining.is_empty() {
            self.outbox.insert(peer.clone(), remaining);
        }
        // A drop always changes the queue. So does a send, but only under the two
        // link states that alter retained state: `Reachable` charges an attempt.
        // `Secure`, `Insecure` and `Unreachable` leave the
        // queue byte-identical, and a flush over them owes storage nothing.
        let mutated = !plan.dropped.is_empty()
            || (!plan.send.is_empty() && matches!(link, LinkState::Reachable));
        if mutated {
            self.dirty = true;
        }
        plan
    }

    /// Drop everything past its TTL, across every peer.
    #[must_use = "expired messages must be surfaced or they stay on 'sending' forever"]
    pub fn cleanup_expired(&mut self, now_unix_secs: u64) -> Vec<DroppedMessage> {
        let mut dropped = Vec::new();
        self.outbox.retain(|peer, queue| {
            queue.retain(|message| {
                if message.is_expired(now_unix_secs) {
                    dropped.push(DroppedMessage {
                        message_id: message.message_id.clone(),
                        peer: peer.clone(),
                        reason: DropReason::Expired,
                    });
                    false
                } else {
                    true
                }
            });
            !queue.is_empty()
        });
        if !dropped.is_empty() {
            self.dirty = true;
        }
        dropped
    }
}
