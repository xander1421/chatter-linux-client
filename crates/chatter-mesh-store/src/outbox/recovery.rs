use super::*;

impl OutboxStore {
    /// Fold a recovered snapshot into memory.
    ///
    /// This is bitchat's `mergeRecoveredOutbox`, and every part of it is load
    /// bearing:
    ///
    /// - **Merge by message ID, never replace.** The recovered copy wins for
    ///   content, but `send_attempts` takes the *maximum* of the two and the
    ///   courier key sets are *unioned* — otherwise the recovered copy re-burns
    ///   courier slots and forgets attempts already spent.
    /// - **Sort by timestamp**, so the FIFO cap evicts by age rather than by
    ///   whichever half of the merge happened to be iterated first.
    /// - **Then apply the cap**, dropping the oldest and reporting them.
    /// - Messages already acked this session are not resurrected, even though the
    ///   durable blob predates the ack.
    #[must_use = "overflow drops must be surfaced or they stay on 'sending' forever"]
    pub fn merge_recovered(&mut self, recovered: Snapshot) -> Vec<DroppedMessage> {
        let mut dropped = Vec::new();
        let mut changed = false;

        for (peer, recovered_queue) in recovered {
            let mut queue = self.outbox.remove(&peer).unwrap_or_default();
            // The timestamp sort below can reorder a queue even when the merge
            // added nothing, and order is what the FIFO cap evicts by, so the
            // dirty flag is decided by comparing the whole queue rather than by
            // watching individual insertions.
            let before = queue.clone();

            for mut candidate in recovered_queue {
                if self.acks.contains(&candidate.message_id) {
                    continue;
                }
                match queue
                    .iter()
                    .position(|m| m.message_id == candidate.message_id)
                {
                    Some(index) => {
                        if let Some(existing) = queue.get(index) {
                            candidate.send_attempts =
                                candidate.send_attempts.max(existing.send_attempts);
                            candidate
                                .deposited_courier_keys
                                .extend(existing.deposited_courier_keys.iter().copied());
                        }
                        if let Some(slot) = queue.get_mut(index) {
                            *slot = candidate;
                        }
                    }
                    None => queue.push(candidate),
                }
            }

            queue.sort_by_key(|m| m.timestamp);

            if queue.len() > MAX_MESSAGES_PER_PEER {
                let overflow = queue.len() - MAX_MESSAGES_PER_PEER;
                for evicted in queue.drain(..overflow) {
                    dropped.push(DroppedMessage {
                        message_id: evicted.message_id,
                        peer: peer.clone(),
                        reason: DropReason::QueueOverflow,
                    });
                }
            }

            if queue != before {
                changed = true;
            }
            if !queue.is_empty() {
                self.outbox.insert(peer, queue);
            }
        }

        if changed {
            self.dirty = true;
        }
        dropped
    }
}
