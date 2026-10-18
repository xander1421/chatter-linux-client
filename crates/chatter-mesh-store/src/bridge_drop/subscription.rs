use super::*;
use chatter_mesh::tlv::courier_envelope::candidate_tags;

impl<R: DropRelay, S: DropSink> BridgeCourier<R, S> {
    // --- Subscription ------------------------------------------------------

    /// Recompute our tag set and (re)open the subscription, then drain the queue.
    ///
    /// Call on toggle changes, on relay connectivity changes, and whenever
    /// [`next_refresh_due`](Self::next_refresh_due) comes around — the periodic call is
    /// what carries the tag set across UTC midnight. Idempotent.
    ///
    /// `local_noise_key` is our own Noise static public key; `None` before an
    /// identity exists, in which case there is nothing to watch.
    pub fn refresh(
        &mut self,
        local_noise_key: Option<[u8; 32]>,
        now_unix_secs: u64,
    ) -> Vec<AbandonedDrop> {
        self.last_refresh = now_unix_secs;

        if !self.bridge_enabled {
            let abandoned = self.cancel_active_publishes();
            self.close_subscription_if_open();
            return abandoned;
        }
        if !self.relay.is_connected() {
            // Keep the tag sets: they are still what an event delivered by a
            // subscription that is only now tearing down should be matched against.
            self.close_subscription_if_open();
            return Vec::new();
        }

        self.my_tags = match local_noise_key {
            Some(key) => candidate_tags(&key, now_unix_secs).to_vec(),
            None => Vec::new(),
        };

        let mut all_tags: Vec<[u8; TAG_LEN]> = self.my_tags.clone();
        all_tags.sort_unstable();
        all_tags.dedup();

        if all_tags.is_empty() {
            self.close_subscription_if_open();
            self.last_subscribed.clear();
            return Vec::new();
        }

        // Resubscribe only when the set actually changed: bitchat's field logs
        // showed the drop subscription being rebuilt every minute or so for an
        // unchanged tag set.
        if !self.subscription_open || all_tags != self.last_subscribed {
            self.relay.open_subscription(&all_tags);
            self.subscription_open = true;
            self.last_subscribed = all_tags;
        }

        self.flush_pending(now_unix_secs)
    }

    /// When [`refresh`](Self::refresh) should next be called, in unix seconds.
    ///
    /// The sans-IO form of the Swift's half-hourly tag-refresh timer. A caller that
    /// only ever refreshes on connectivity changes still works; it just discovers the
    /// day rollover late.
    pub fn next_refresh_due(&self) -> u64 {
        self.last_refresh.saturating_add(REFRESH_INTERVAL_SECS)
    }

    // --- Inbound drops -----------------------------------------------------

    /// Handle one drop event delivered by the subscription.
    ///
    /// The event's signature has already been verified by the relay layer; everything
    /// this checks is about whether the drop is ours to act on.
    pub fn handle_drop(&mut self, event: &DropEvent<'_>, now_unix_secs: u64) -> DropDisposition {
        if !self.bridge_enabled {
            return DropDisposition::Ignored(IgnoreReason::BridgeDisabled);
        }
        // A resubscribe can still deliver an event from the old tag set, so the
        // event ID is not consumed until the drop is actually opened.
        if self
            .seen_drop_event_ids
            .contains(event.event_id, now_unix_secs)
        {
            return DropDisposition::Ignored(IgnoreReason::AlreadyHandled);
        }
        if event.envelope_bytes.len() > MAX_DROP_ENVELOPE_BYTES {
            return DropDisposition::Ignored(IgnoreReason::Malformed);
        }
        let Ok(envelope) = CourierEnvelope::decode(event.envelope_bytes) else {
            return DropDisposition::Ignored(IgnoreReason::Malformed);
        };
        if envelope.is_expired(millis(now_unix_secs)) {
            return DropDisposition::Ignored(IgnoreReason::Expired);
        }
        // The envelope's own tag must agree with the tag the event was filed under, or
        // a mislabelled drop could ride a subscription it does not belong to.
        if envelope.recipient_tag != event.tag {
            return DropDisposition::Ignored(IgnoreReason::Mislabelled);
        }

        if !self.my_tags.contains(&event.tag) {
            return DropDisposition::Ignored(IgnoreReason::NotWatched);
        }
        if self.sink.open_for_self(&envelope) {
            self.seen_drop_event_ids
                .insert(event.event_id, now_unix_secs);
            DropDisposition::OpenedForSelf
        } else {
            DropDisposition::Retryable
        }
    }

    fn close_subscription_if_open(&mut self) {
        if self.subscription_open {
            self.relay.close_subscription();
            self.subscription_open = false;
        }
    }
}
