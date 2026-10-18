use super::*;

impl ConnectionScheduler {
    // -----------------------------------------------------------------------
    // Outcomes
    // -----------------------------------------------------------------------

    /// Record that a connect was just issued. This is what the global rate limit
    /// measures from, so it must be called for *every* attempt, whatever its origin.
    pub fn note_connect_attempt(&mut self, now_ms: u64) {
        self.last_connect_attempt_ms = Some(now_ms);
    }

    /// A link came up. Clears every form of backoff for that address.
    pub fn note_connected(&mut self, address: &str) {
        self.failure_counts.remove(address);
        self.recent_connect_timeouts.remove(address);
        self.recent_connect_failures.remove(address);
        self.recent_disconnects.remove(address);
    }

    /// A connect attempt failed outright (as opposed to timing out).
    ///
    /// Counts against the candidate's score, and — unlike iOS — earns a short
    /// per-address window and an attempt cap. See
    /// [`CONNECT_FAILURE_RETRY_DELAY_MS`].
    pub fn note_connect_failed(&mut self, address: &str, now_ms: u64) {
        *self.failure_counts.entry(address.to_string()).or_insert(0) += 1;

        let entry = self
            .recent_connect_failures
            .entry(address.to_string())
            .or_insert((0, now_ms));
        // A record we have not touched in a while is stale: the peer was
        // probably just out of range, and starting the count again is what stops
        // one bad patch of radio from blacklisting it for the session.
        let expired = now_ms.saturating_sub(entry.1) > CONNECT_FAILURE_FORGET_MS;
        entry.0 = if expired {
            1
        } else {
            entry.0.saturating_add(1)
        };
        entry.1 = now_ms;
    }

    /// Whether this address should be left alone because its recent attempts keep
    /// failing.
    ///
    /// Two reasons to say yes: the last failure is too recent, or it has failed
    /// [`MAX_CONNECT_FAILURES`] times in a row and the record has not yet aged
    /// out. Public so the radio layer can answer the same question before it
    /// spends a connect slot.
    #[must_use]
    pub fn is_failing_too_fast(&self, address: &str, now_ms: u64) -> bool {
        let Some(&(count, last_ms)) = self.recent_connect_failures.get(address) else {
            return false;
        };
        let since = now_ms.saturating_sub(last_ms);
        if since > CONNECT_FAILURE_FORGET_MS {
            return false;
        }
        since < CONNECT_FAILURE_RETRY_DELAY_MS || count >= MAX_CONNECT_FAILURES
    }

    /// An established link dropped. Earns the short settle window, not the timeout
    /// backoff — the peer probably walked around a corner and will be back.
    pub fn note_disconnected(&mut self, address: &str, now_ms: u64) {
        self.recent_disconnects.insert(address.to_string(), now_ms);
    }

    /// A connect attempt ran past [`CONNECT_TIMEOUT_MS`] without completing. Earns
    /// both the long ignore window and a failure count.
    pub fn note_connect_timeout(&mut self, address: &str, now_ms: u64) {
        self.recent_connect_timeouts
            .insert(address.to_string(), now_ms);
        self.note_connect_failed(address, now_ms);
    }

    /// This address turned out to be another rotation of a peer we already hold.
    ///
    /// Deliberately not `note_connect_failed`: nothing failed, and a failure count
    /// would feed the backoff that governs genuinely broken peers.
    pub fn note_duplicate_peer(&mut self, address: &str, now_ms: u64) {
        self.duplicate_peers.insert(address.to_string(), now_ms);
    }

    /// Garbage-collect per-peer backoff older than
    /// [`CONNECT_TIMEOUT_BACKOFF_WINDOW_MS`]. Call it on the maintenance tick.
    ///
    /// Failure counts for peers with no surviving record and no queued candidate go
    /// too. bitchat keeps those forever, which leaks slowly but genuinely against
    /// rotating BLE addresses; the penalty they carry is at most 16 points, under
    /// 8 dBm of equivalent signal, so forgetting it past the backoff window costs
    /// nothing and bounds the map.
    pub fn prune_backoff(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(CONNECT_TIMEOUT_BACKOFF_WINDOW_MS);
        self.recent_connect_timeouts
            .retain(|_, &mut at| at >= cutoff);
        self.recent_connect_failures
            .retain(|_, &mut (_, at)| now_ms.saturating_sub(at) <= CONNECT_FAILURE_FORGET_MS);
        self.recent_disconnects.retain(|_, &mut at| at >= cutoff);
        self.duplicate_peers
            .retain(|_, &mut at| now_ms.saturating_sub(at) < DUPLICATE_PEER_IGNORE_MS);

        let timeouts = &self.recent_connect_timeouts;
        let disconnects = &self.recent_disconnects;
        let candidates = &self.candidates;
        self.failure_counts.retain(|address, _| {
            timeouts.contains_key(address)
                || disconnects.contains_key(address)
                || candidates.iter().any(|c| &c.address == address)
        });
    }
    /// `Some(delay)` while the global connect rate limit is in force.
    pub(super) fn rate_limit_retry_delay(&self, now_ms: u64) -> Option<u64> {
        let last = self.last_connect_attempt_ms?;
        let elapsed = now_ms.saturating_sub(last);
        if elapsed >= self.config.connect_rate_limit_ms {
            return None;
        }
        Some(self.config.connect_rate_limit_ms - elapsed + RATE_LIMIT_RETRY_SLACK_MS)
    }

    /// `Some(delay)` when this candidate timed out recently *and* is still weak.
    /// A peer that timed out but is now loud gets another go immediately.
    pub(super) fn weak_link_retry_delay(
        &self,
        candidate: &ConnectionCandidate,
        now_ms: u64,
    ) -> Option<u64> {
        let last_timeout = *self.recent_connect_timeouts.get(&candidate.address)?;
        let elapsed = now_ms.saturating_sub(last_timeout);
        if elapsed >= self.config.weak_link_cooldown_ms
            || candidate.rssi > self.config.weak_link_rssi_cutoff
        {
            return None;
        }
        let remaining = self.config.weak_link_cooldown_ms - elapsed;
        Some(remaining.clamp(WEAK_LINK_RETRY_MIN_MS, WEAK_LINK_RETRY_MAX_MS))
    }

    /// `Some(delay)` while this candidate is inside its post-disconnect settle window.
    ///
    /// This has to hold on the drain path too, not just on discovery: a candidate
    /// queued while the device was still connected would otherwise reconnect the
    /// instant the link dropped, bypassing the window and recreating the exact
    /// reconnect/cancel thrash it exists to prevent.
    pub(super) fn disconnect_settle_delay(
        &self,
        candidate: &ConnectionCandidate,
        now_ms: u64,
    ) -> Option<u64> {
        let last_disconnect = *self.recent_disconnects.get(&candidate.address)?;
        let elapsed = now_ms.saturating_sub(last_disconnect);
        if elapsed >= DISCONNECT_DISCOVERY_IGNORE_MS {
            return None;
        }
        Some(DISCONNECT_DISCOVERY_IGNORE_MS - elapsed + DISCONNECT_SETTLE_SLACK_MS)
    }
}
