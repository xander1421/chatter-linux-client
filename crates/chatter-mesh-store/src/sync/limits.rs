use super::{MAX_RESPONSES_PER_WINDOW, RESPONSE_RATE_LIMIT_WINDOW_SECS, RESPONSE_WINDOW_SECS};
use chatter_mesh::ids::PeerId;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Outstanding requests
// ---------------------------------------------------------------------------

/// Tracks which peers we asked for sync, so their answers can be told apart from
/// an attacker's replay.
///
/// Ported from `RequestSyncManager.swift`. The whole point is stated in
/// `docs/REQUEST_SYNC_MANAGER.md`: the RSR flag exempts a packet from timestamp
/// validation, and an exemption an attacker can claim for itself is not an
/// exemption, it is a hole. Only a peer we sent a request to, within the window,
/// gets the exemption.
///
/// One timestamp per peer, not one per request: a single request is answered by
/// many packets, and each of them must pass.
#[derive(Clone, Debug)]
pub struct RequestSyncTracker {
    window_secs: u64,
    /// Peer to the time we last asked it.
    pending: BTreeMap<PeerId, u64>,
}

impl Default for RequestSyncTracker {
    fn default() -> Self {
        Self::new(RESPONSE_WINDOW_SECS)
    }
}

impl RequestSyncTracker {
    pub fn new(window_secs: u64) -> Self {
        Self {
            window_secs,
            pending: BTreeMap::new(),
        }
    }

    /// Records that we are sending a sync request to `peer`.
    ///
    /// A second request to the same peer refreshes the timestamp rather than
    /// stacking, which is what makes the window slide with our own requests
    /// instead of expiring mid-answer.
    pub fn register_request(&mut self, peer: &PeerId, now_unix_secs: u64) {
        self.pending.insert(peer.clone(), now_unix_secs);
    }

    /// Whether an inbound packet is a valid answer to something we asked for.
    ///
    /// A non-RSR packet is never "a response" — it takes the ordinary freshness
    /// path and this returns false for it, which is not a rejection.
    pub fn is_valid_response(&self, peer: &PeerId, is_rsr: bool, now_unix_secs: u64) -> bool {
        is_rsr && self.awaits_answer_from(peer, now_unix_secs)
    }

    /// Whether a request to `peer` is outstanding, whatever the answer's flags.
    pub fn awaits_answer_from(&self, peer: &PeerId, now_unix_secs: u64) -> bool {
        let Some(requested_at) = self.pending.get(peer) else {
            // A peer we never asked.
            return false;
        };
        // Saturating: a clock that jumped backwards makes the age zero rather than
        // wrapping to something enormous, so a legitimate answer survives it.
        now_unix_secs.saturating_sub(*requested_at) <= self.window_secs
    }

    /// Drops requests whose window has closed.
    pub fn cleanup(&mut self, now_unix_secs: u64) {
        let window = self.window_secs;
        self.pending
            .retain(|_, requested_at| now_unix_secs.saturating_sub(*requested_at) <= window);
    }

    /// The peers we have outstanding sync requests to.
    ///
    /// Diagnostic only. An unsolicited-response rejection while this is non-empty
    /// means the key we registered and the sender we looked up disagree, which
    /// the count alone cannot tell you.
    pub fn pending_peers(&self) -> Vec<PeerId> {
        self.pending.keys().cloned().collect()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

// ---------------------------------------------------------------------------
// Response rate limiting
// ---------------------------------------------------------------------------

/// Sliding-window limiter for `REQUEST_SYNC` responses, per peer.
///
/// Ported from `SyncResponseRateLimiter.swift`. A single response pass can replay
/// the entire gossip store, so a peer that requests in a tight loop must not be able
/// to drain the airtime and battery of everyone in radio range. Legitimate peers
/// send at most a few requests per maintenance tick — one per due type schedule,
/// plus the initial sync — so eight per thirty seconds is generous.
#[derive(Clone, Debug)]
pub struct SyncResponseRateLimiter {
    max_responses: usize,
    window_secs: u64,
    history: BTreeMap<PeerId, Vec<u64>>,
}

impl Default for SyncResponseRateLimiter {
    fn default() -> Self {
        Self::new(MAX_RESPONSES_PER_WINDOW, RESPONSE_RATE_LIMIT_WINDOW_SECS)
    }
}

impl SyncResponseRateLimiter {
    pub fn new(max_responses: usize, window_secs: u64) -> Self {
        Self {
            // A zero budget would mute sync entirely; Swift clamps the same way.
            max_responses: max_responses.max(1),
            window_secs,
            history: BTreeMap::new(),
        }
    }

    /// Whether `peer` is still under its budget — and, if so, records the response.
    pub fn should_respond(&mut self, peer: &PeerId, now_unix_secs: u64) -> bool {
        let cutoff = now_unix_secs.saturating_sub(self.window_secs);
        let recent = self.history.entry(peer.clone()).or_default();
        // Inclusive of the cutoff, matching Swift's `>=`: an entry exactly one
        // window old still counts against the budget.
        recent.retain(|at| *at >= cutoff);
        if recent.len() >= self.max_responses {
            return false;
        }
        recent.push(now_unix_secs);
        true
    }

    /// Drops history outside the window so departed peers do not accumulate.
    pub fn prune(&mut self, now_unix_secs: u64) {
        let cutoff = now_unix_secs.saturating_sub(self.window_secs);
        self.history.retain(|_, times| {
            times.retain(|at| *at >= cutoff);
            !times.is_empty()
        });
    }

    /// Responses recorded for `peer` inside the current window.
    pub fn recorded(&self, peer: &PeerId) -> usize {
        self.history.get(peer).map_or(0, Vec::len)
    }
}
