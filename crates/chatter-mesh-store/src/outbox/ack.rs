use super::*;

/// Bounded memory of recently acked message IDs.
pub(super) struct AckMemory {
    kinds: BTreeMap<String, AckKind>,
    order: VecDeque<String>,
}

impl AckMemory {
    pub(super) fn new() -> Self {
        Self {
            kinds: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Records an ack and returns the strongest one seen for this ID.
    pub(super) fn record(&mut self, message_id: &str, kind: AckKind) -> AckKind {
        match self.kinds.get_mut(message_id) {
            Some(existing) => {
                // Never downgrade. A read ack outranks a delivery ack whatever
                // order the two arrive in.
                *existing = (*existing).max(kind);
                *existing
            }
            None => {
                self.kinds.insert(message_id.to_owned(), kind);
                self.order.push_back(message_id.to_owned());
                while self.order.len() > ACK_MEMORY_LIMIT {
                    if let Some(evicted) = self.order.pop_front() {
                        self.kinds.remove(&evicted);
                    }
                }
                kind
            }
        }
    }

    pub(super) fn contains(&self, message_id: &str) -> bool {
        self.kinds.contains_key(message_id)
    }

    pub(super) fn strongest(&self, message_id: &str) -> Option<AckKind> {
        self.kinds.get(message_id).copied()
    }

    pub(super) fn clear(&mut self) {
        self.kinds.clear();
        self.order.clear();
    }
}

impl OutboxStore {
    /// A delivery or read ack from `from`: stop retaining that peer's copy.
    ///
    /// Both kinds clear it. The ack is remembered either way, so that a cold-load
    /// merge cannot resurrect it and a later delivery ack cannot downgrade a read
    /// ack — see [`AckOutcome::strongest`].
    ///
    /// `from` is load-bearing. This used to `retain` across EVERY peer's queue,
    /// so an ack cleared any message with that ID no matter who it was addressed
    /// to. Acks arrive Noise-decrypted, so a peer can only ack as itself — but it
    /// could still cancel a message queued for somebody else, and the sender was
    /// told "delivered" for mail that was never sent. Only the acking peer's own
    /// queue may be touched. OPEN-ITEMS §100.
    pub fn mark_acked(&mut self, from: &PeerId, message_id: &str, kind: AckKind) -> AckOutcome {
        let strongest = self.acks.record(message_id, kind);

        let mut cleared = false;
        if let Some(queue) = self.outbox.get_mut(from) {
            let before = queue.len();
            queue.retain(|m| m.message_id != message_id);
            if queue.len() != before {
                cleared = true;
            }
            if queue.is_empty() {
                self.outbox.remove(from);
            }
        }
        if cleared {
            self.dirty = true;
        }

        AckOutcome { cleared, strongest }
    }
}
