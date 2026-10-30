use super::*;

impl ConnectionScheduler {
    // -----------------------------------------------------------------------
    // The moving RSSI gate
    // -----------------------------------------------------------------------

    /// Recompute the RSSI gate and return it.
    ///
    /// - **No connected peers:** [`RSSI_ISOLATED_BASE`], relaxing to
    ///   [`RSSI_ISOLATED_RELAXED`] once we have been alone for longer than
    ///   [`ISOLATION_RELAX_THRESHOLD_MS`]. This is the part that matters: a node with
    ///   nobody in earshot has to reach further or it never joins the mesh at all,
    ///   and a fringe link it can barely hold still beats no link.
    /// - **Peers, and capacity to spare:** [`DYNAMIC_RSSI_THRESHOLD_DEFAULT`].
    /// - **Peers, but the budget or candidate queue is full:**
    ///   [`RSSI_CONNECTED_THRESHOLD`] — only a clearly better link is worth the churn.
    ///
    /// Note what it deliberately does *not* do: repeated timeouts never tighten the
    /// gate. Flaky peers are punished individually, because one distant flapping
    /// device must not blind us to every other edge-of-range peer.
    ///
    /// `connected_count` is peers with a live link; `connected_or_connecting_links`
    /// counts outbound links against the budget, including in-flight connects.
    pub fn update_rssi_threshold(
        &mut self,
        connected_count: usize,
        connected_or_connecting_links: usize,
        now_ms: u64,
    ) -> i16 {
        if connected_count == 0 {
            let isolated_since = *self.isolated_since_ms.get_or_insert(now_ms);
            let elapsed = now_ms.saturating_sub(isolated_since);
            self.dynamic_rssi_threshold = if elapsed > ISOLATION_RELAX_THRESHOLD_MS {
                RSSI_ISOLATED_RELAXED
            } else {
                RSSI_ISOLATED_BASE
            };
            return self.dynamic_rssi_threshold;
        }

        self.isolated_since_ms = None;

        let saturated = connected_or_connecting_links >= self.config.max_central_links
            || self.candidates.len() >= self.config.candidate_cap;
        self.dynamic_rssi_threshold = if saturated {
            RSSI_CONNECTED_THRESHOLD
        } else {
            DYNAMIC_RSSI_THRESHOLD_DEFAULT
        };
        self.dynamic_rssi_threshold
    }

    /// How long we have been without a peer, if we are currently isolated.
    pub fn isolated_for_ms(&self, now_ms: u64) -> Option<u64> {
        self.isolated_since_ms
            .map(|since| now_ms.saturating_sub(since))
    }
}
