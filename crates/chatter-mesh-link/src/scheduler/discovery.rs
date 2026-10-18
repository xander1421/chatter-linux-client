use super::*;

impl ConnectionScheduler {
    // -----------------------------------------------------------------------
    // Discovery
    // -----------------------------------------------------------------------

    /// Decide what an advert is worth.
    ///
    /// The order of the checks is load-bearing. RSSI and budget are tested *before*
    /// link state, so a weak advert for a device we are already connected to is
    /// queued rather than ignored — which is why
    /// [`next_candidate`](Self::next_candidate) has to re-check the disconnect settle
    /// window when it drains.
    pub fn handle_discovery(
        &mut self,
        candidate: &ConnectionCandidate,
        connected_or_connecting: usize,
        existing: Option<ExistingConnectionState>,
        peripheral_state: PeripheralConnectionState,
        now_ms: u64,
    ) -> DiscoveryDecision {
        if !candidate.is_connectable {
            return DiscoveryDecision::Ignore;
        }

        if candidate.rssi <= self.dynamic_rssi_threshold {
            self.enqueue(candidate.clone());
            return DiscoveryDecision::Queued;
        }

        if connected_or_connecting >= self.config.max_central_links {
            self.enqueue(candidate.clone());
            return DiscoveryDecision::Queued;
        }

        if let Some(after_ms) = self.rate_limit_retry_delay(now_ms) {
            self.enqueue(candidate.clone());
            return DiscoveryDecision::ScheduleRetry { after_ms };
        }

        if let Some(existing) = existing {
            if existing.is_connected || existing.is_connecting {
                return DiscoveryDecision::Ignore;
            }
            if let Some(last_attempt) = existing.last_connect_attempt_ms {
                if now_ms.saturating_sub(last_attempt) < RECENT_ATTEMPT_IGNORE_MS {
                    return DiscoveryDecision::Ignore;
                }
            }
        }

        if let Some(&last_timeout) = self.recent_connect_timeouts.get(&candidate.address) {
            if now_ms.saturating_sub(last_timeout) < TIMEOUT_DISCOVERY_IGNORE_MS {
                return DiscoveryDecision::Ignore;
            }
        }

        if self.is_failing_too_fast(&candidate.address, now_ms) {
            return DiscoveryDecision::Ignore;
        }

        if let Some(&noted) = self.duplicate_peers.get(&candidate.address) {
            if now_ms.saturating_sub(noted) < DUPLICATE_PEER_IGNORE_MS {
                return DiscoveryDecision::Ignore;
            }
        }

        if let Some(&last_disconnect) = self.recent_disconnects.get(&candidate.address) {
            if now_ms.saturating_sub(last_disconnect) < DISCONNECT_DISCOVERY_IGNORE_MS {
                return DiscoveryDecision::Ignore;
            }
        }

        match peripheral_state {
            PeripheralConnectionState::Disconnected => DiscoveryDecision::ConnectNow,
            PeripheralConnectionState::Connecting | PeripheralConnectionState::Connected => {
                DiscoveryDecision::CancelStaleConnection
            }
        }
    }

    /// Shorthand over [`handle_discovery`](Self::handle_discovery) for the common
    /// radio-layer case: a `LinkEvent::Discovered` for an address we hold no link to.
    ///
    /// Returns `true` when the radio should issue a connect right now. Every other
    /// outcome (queued, rate-limited, backed off) returns `false` and is handled by
    /// the next [`next_candidate`](Self::next_candidate) drain — so a `false` here is
    /// never a dropped opportunity, just a deferred one.
    pub fn should_connect(
        &mut self,
        address: &str,
        rssi: i16,
        connected_or_connecting: usize,
        now_ms: u64,
    ) -> bool {
        let candidate = ConnectionCandidate::new(address, rssi, now_ms);
        let decision = self.handle_discovery(
            &candidate,
            connected_or_connecting,
            None,
            PeripheralConnectionState::Disconnected,
            now_ms,
        );
        decision == DiscoveryDecision::ConnectNow
    }
}
