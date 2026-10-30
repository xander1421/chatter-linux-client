use std::collections::VecDeque;

// --- Expiring ID set -------------------------------------------------------

/// A bounded, ageing set of IDs.
///
/// [`chatter_mesh::dedup::MessageDeduplicator`] is the same idea, but it cannot hand
/// its entries back out and its `contains` ignores the age — and both matter here,
/// because this record is persisted across relaunches and queried without recording.
pub(super) struct ExpiringIdSet {
    entries: VecDeque<(String, u64)>,
    capacity: usize,
    lifetime_secs: u64,
}

impl ExpiringIdSet {
    pub(super) fn new(capacity: usize, lifetime_secs: u64) -> Self {
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
    pub(super) fn contains(&self, id: &str, now: u64) -> bool {
        self.entries
            .iter()
            .any(|(entry, at)| entry == id && self.is_live(*at, now))
    }

    pub(super) fn insert(&mut self, id: &str, now: u64) {
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

    pub(super) fn entries(&self) -> Vec<(String, u64)> {
        self.entries.iter().cloned().collect()
    }

    pub(super) fn restore(&mut self, entries: &[(String, u64)], now: u64) {
        for (id, at) in entries {
            if self.is_live(*at, now) {
                self.insert(id, *at);
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}
