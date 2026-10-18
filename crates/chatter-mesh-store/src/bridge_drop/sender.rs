use super::*;

impl<R: DropRelay, S: DropSink> BridgeCourier<R, S> {
    // --- Sender role -------------------------------------------------------

    /// Publish a sealed copy of outbound mail as a relay drop.
    ///
    /// Called by the router alongside its physical courier deposits — the two are
    /// parallel, not alternatives, and a message delivered twice is deduplicated
    /// downstream. Idempotent per message ID: a second deposit while the first is
    /// queued, in flight, or already accepted is refused rather than duplicated.
    pub fn deposit(
        &mut self,
        message_id: &str,
        envelope: &CourierEnvelope,
        now_unix_secs: u64,
    ) -> Deposited {
        let refuse = |reason| Deposited {
            outcome: DepositOutcome::Refused(reason),
            abandoned: Vec::new(),
        };

        if !self.bridge_enabled {
            return refuse(DepositRefusal::BridgeDisabled);
        }
        if self.published_drop_keys.contains(message_id, now_unix_secs) {
            return refuse(DepositRefusal::AlreadyPublished);
        }
        if self.rejected_drop_keys.contains(message_id, now_unix_secs) {
            return refuse(DepositRefusal::Undeliverable);
        }
        if self.is_tracked(message_id) {
            return refuse(DepositRefusal::AlreadyInFlight);
        }

        // An envelope that cannot encode within the drop caps fails identically on
        // every attempt — the size is a function of the content, not of the sealing.
        // Suppress it for this process so the retry sweep does not churn, but never
        // record it as published: no relay ever saw it.
        match envelope.encode() {
            Ok(encoded) if encoded.len() <= MAX_DROP_ENVELOPE_BYTES => {}
            _ => {
                self.rejected_drop_keys.insert(message_id, now_unix_secs);
                return refuse(DepositRefusal::Undeliverable);
            }
        }
        if expires_at_secs(envelope, now_unix_secs).is_none() {
            return refuse(DepositRefusal::Expired);
        }

        let operation = self.mint_operation();
        let attempt = self.attempt_publish(
            operation,
            message_id.to_owned(),
            envelope.clone(),
            now_unix_secs,
        );
        let outcome = match attempt.state {
            AttemptState::Published => DepositOutcome::Publishing(operation),
            AttemptState::Queued => DepositOutcome::Queued(operation),
            AttemptState::Dropped => DepositOutcome::Refused(DepositRefusal::Undeliverable),
        };
        Deposited {
            outcome,
            abandoned: attempt.abandoned,
        }
    }

    /// Report a relay's verdict on a publish attempt.
    ///
    /// `accepted` must mean a relay explicitly acknowledged the event (NIP-20 `OK`).
    /// Only that commits the sender-side dedup key.
    ///
    /// Returns `None` for an operation this instance is not waiting on: a duplicated
    /// callback, one invalidated by a wipe or a bridge disable, or one belonging to a
    /// superseded attempt. Completing a newer attempt from an older callback is
    /// exactly the confusion the operation ID exists to prevent.
    pub fn resolve_publish(
        &mut self,
        operation: DropOperationId,
        accepted: bool,
        now_unix_secs: u64,
    ) -> Option<ResolvedDrop> {
        let index = self
            .in_flight
            .iter()
            .position(|entry| entry.operation == operation)?;
        let entry = self.in_flight.remove(index);

        if accepted {
            self.published_drop_keys
                .insert(&entry.message_id, now_unix_secs);
        }

        Some(ResolvedDrop {
            operation,
            message_id: entry.message_id,
            accepted,
        })
    }

    /// Publish the drops that were queued while relays were unreachable.
    ///
    /// Each one is a fresh attempt: re-encoded, re-checked against the caps and the
    /// expiry, and signed with a new throwaway key.
    pub fn flush_pending(&mut self, now_unix_secs: u64) -> Vec<AbandonedDrop> {
        if !self.bridge_enabled || !self.relay.is_connected() || self.pending.is_empty() {
            return Vec::new();
        }
        let queued: Vec<PendingDrop> = self.pending.drain(..).collect();
        let mut abandoned = Vec::new();
        for item in queued {
            let attempt = self.attempt_publish(
                item.operation,
                item.message_id,
                item.envelope,
                now_unix_secs,
            );
            abandoned.extend(attempt.abandoned);
        }
        abandoned
    }

    // --- Internals ---------------------------------------------------------

    pub(super) fn mint_operation(&mut self) -> DropOperationId {
        let id = DropOperationId(self.next_operation);
        self.next_operation = self.next_operation.wrapping_add(1);
        id
    }

    /// One publish attempt: publish now, queue for later, or give up.
    ///
    /// The throwaway key is requested here and moved straight into the request, so
    /// every attempt — including every retry of the same envelope — signs under a key
    /// nobody has seen before.
    pub(super) fn attempt_publish(
        &mut self,
        operation: DropOperationId,
        message_id: String,
        envelope: CourierEnvelope,
        now_unix_secs: u64,
    ) -> AttemptResult {
        let dropped = AttemptResult {
            state: AttemptState::Dropped,
            abandoned: Vec::new(),
        };

        let Ok(encoded) = envelope.encode() else {
            return dropped;
        };
        if encoded.len() > MAX_DROP_ENVELOPE_BYTES {
            return dropped;
        }
        let Some(expires_at_unix_secs) = expires_at_secs(&envelope, now_unix_secs) else {
            return dropped;
        };

        if !self.relay.is_connected() {
            return AttemptResult {
                state: AttemptState::Queued,
                abandoned: self.enqueue(operation, message_id, envelope),
            };
        }

        self.in_flight.push(InFlightDrop {
            operation,
            message_id,
        });
        self.relay.publish(PublishRequest {
            operation,
            recipient_tag: envelope.recipient_tag,
            sealed_envelope: &encoded,
            expires_at_unix_secs,
            publisher_key: self.relay.fresh_publisher_key(),
        });
        AttemptResult {
            state: AttemptState::Published,
            abandoned: Vec::new(),
        }
    }

    /// Queue a drop, evicting the oldest once the bound is exceeded.
    pub(super) fn enqueue(
        &mut self,
        operation: DropOperationId,
        message_id: String,
        envelope: CourierEnvelope,
    ) -> Vec<AbandonedDrop> {
        self.pending.push_back(PendingDrop {
            operation,
            message_id,
            envelope,
        });
        let mut abandoned = Vec::new();
        while self.pending.len() > MAX_PENDING_DROPS {
            if let Some(evicted) = self.pending.pop_front() {
                abandoned.push(AbandonedDrop {
                    operation: evicted.operation,
                    message_id: evicted.message_id,
                });
            }
        }
        abandoned
    }

    /// Whether a message ID already has an attempt queued or awaiting a relay result.
    pub(super) fn is_tracked(&self, message_id: &str) -> bool {
        let queued = self
            .pending
            .iter()
            .any(|item| item.message_id == message_id);
        let in_flight = self
            .in_flight
            .iter()
            .any(|entry| entry.message_id == message_id);
        queued || in_flight
    }

    /// Invalidate everything queued or awaiting a relay result. Late callbacks for
    /// these operations find no entry and no-op.
    pub(super) fn cancel_active_publishes(&mut self) -> Vec<AbandonedDrop> {
        let mut abandoned: Vec<AbandonedDrop> = self
            .pending
            .drain(..)
            .map(|item| AbandonedDrop {
                operation: item.operation,
                message_id: item.message_id,
            })
            .collect();
        abandoned.extend(self.in_flight.drain(..).map(|entry| AbandonedDrop {
            operation: entry.operation,
            message_id: entry.message_id,
        }));
        abandoned
    }
}
