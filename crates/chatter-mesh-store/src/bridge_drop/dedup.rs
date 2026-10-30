use super::*;

impl<R: DropRelay, S: DropSink> BridgeCourier<R, S> {
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
}
