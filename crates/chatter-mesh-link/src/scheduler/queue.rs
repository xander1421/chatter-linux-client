use super::*;

impl ConnectionScheduler {
    /// Add or refresh a candidate, keeping the queue sorted and capped.
    ///
    /// Sort is by signal strength, ties to whoever we saw first; the overflow is
    /// dropped from the weak end. Address is the final tiebreak so the queue is
    /// deterministic — Swift's sort is unstable and leaves exact ties in arbitrary
    /// order, which is untestable and pointless to reproduce.
    pub fn enqueue(&mut self, candidate: ConnectionCandidate) {
        match self
            .candidates
            .iter_mut()
            .find(|existing| existing.address == candidate.address)
        {
            Some(existing) => *existing = candidate,
            None => self.candidates.push(candidate),
        }

        self.candidates.sort_by(|a, b| {
            b.rssi
                .cmp(&a.rssi)
                .then(a.discovered_at_ms.cmp(&b.discovered_at_ms))
                .then(a.address.cmp(&b.address))
        });
        self.candidates.truncate(self.config.candidate_cap);
    }

    /// Drop a candidate outright, e.g. once a link to it is established by the
    /// peripheral role and a second outbound link would be redundant.
    pub fn remove_candidate(&mut self, address: &str) {
        self.candidates.retain(|c| c.address != address);
    }

    // -----------------------------------------------------------------------
    // Draining the queue
    // -----------------------------------------------------------------------

    /// Pick the next device to connect to, if any.
    ///
    /// `is_busy` answers "am I already connected or connecting to this address?" —
    /// the link registry knows, the scheduler does not.
    ///
    /// Candidates are ranked by [`Self::score`]-equivalent ordering: signal first,
    /// discounted by how stale the advert is and by how badly this peer has behaved.
    pub fn next_candidate(
        &mut self,
        connected_or_connecting: usize,
        is_busy: impl Fn(&str) -> bool,
        now_ms: u64,
    ) -> QueueDecision {
        if connected_or_connecting >= self.config.max_central_links {
            return QueueDecision::Idle;
        }

        if let Some(after_ms) = self.rate_limit_retry_delay(now_ms) {
            return QueueDecision::RetryAfter { after_ms };
        }

        // Scoring depends only on `now_ms` and on state that cannot change inside
        // this loop, so one sort is equivalent to Swift's re-sort per iteration.
        self.sort_by_score(now_ms);

        while !self.candidates.is_empty() {
            let candidate = self.candidates.remove(0);

            if !candidate.is_connectable {
                continue;
            }

            if let Some(after_ms) = self.weak_link_retry_delay(&candidate, now_ms) {
                self.enqueue(candidate);
                return QueueDecision::RetryAfter { after_ms };
            }

            if let Some(after_ms) = self.disconnect_settle_delay(&candidate, now_ms) {
                self.enqueue(candidate);
                return QueueDecision::RetryAfter { after_ms };
            }

            if is_busy(&candidate.address) {
                continue;
            }

            return QueueDecision::Connect(candidate);
        }

        QueueDecision::Idle
    }

    /// Best first. Ties break on the same keys [`Self::enqueue`] uses, for
    /// determinism.
    fn sort_by_score(&mut self, now_ms: u64) {
        let mut candidates = std::mem::take(&mut self.candidates);
        candidates.sort_by(|a, b| {
            self.score(b, now_ms)
                .cmp(&self.score(a, now_ms))
                .then(a.discovered_at_ms.cmp(&b.discovered_at_ms))
                .then(a.address.cmp(&b.address))
        });
        self.candidates = candidates;
    }

    /// Rank a candidate: connectable and loud is good, stale and flaky is not.
    ///
    /// Signal dominates (2 points per dBm), staleness costs 1 point per 100 ms, and
    /// a peer's own history costs at most 26 — deliberately small, so history nudges
    /// the order without ever letting a distant peer beat a close one.
    pub fn score(&self, candidate: &ConnectionCandidate, now_ms: u64) -> i64 {
        let failures = self.failure_count(&candidate.address);
        let shift = failures.min(FAILURE_PENALTY_MAX_SHIFT);
        let penalty = i64::from(1u32 << shift).min(FAILURE_PENALTY_CAP);

        let timeout_bias = match self.recent_connect_timeouts.get(&candidate.address) {
            Some(&at) if now_ms.saturating_sub(at) < TIMEOUT_SCORE_BIAS_WINDOW_MS => {
                TIMEOUT_SCORE_BIAS
            }
            _ => 0,
        };

        let connectable = if candidate.is_connectable {
            CONNECTABLE_SCORE_BONUS
        } else {
            0
        };
        let base = connectable + (i64::from(candidate.rssi) + 100) * 2;

        // Signed on purpose: an advert timestamped slightly ahead of `now_ms` (clock
        // skew between the radio's stamp and ours) scores as fresh rather than
        // saturating to zero age.
        let age_ms = now_ms as i64 - candidate.discovered_at_ms as i64;
        let recency = -(age_ms / 100);

        base + recency - penalty - timeout_bias
    }
}
