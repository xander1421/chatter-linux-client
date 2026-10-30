use super::*;
use std::cell::RefCell;

const DAY: u64 = 86_400;
const NOW: u64 = 1_700_000_000;

// --- Fakes -------------------------------------------------------------

#[derive(Debug)]
struct SentDrop {
    operation: DropOperationId,
    tag: [u8; TAG_LEN],
    envelope: Vec<u8>,
    expires_at: u64,
    publisher_key: [u8; 32],
}

#[derive(Default)]
struct FakeRelay {
    connected: RefCell<bool>,
    keys_minted: RefCell<u64>,
    published: RefCell<Vec<SentDrop>>,
    subscriptions: RefCell<Vec<Vec<[u8; TAG_LEN]>>>,
    closes: RefCell<usize>,
}

impl FakeRelay {
    fn set_connected(&self, connected: bool) {
        *self.connected.borrow_mut() = connected;
    }

    fn publish_count(&self) -> usize {
        self.published.borrow().len()
    }

    fn last_operation(&self) -> DropOperationId {
        self.published.borrow().last().expect("a publish").operation
    }

    fn publisher_keys(&self) -> Vec<[u8; 32]> {
        self.published
            .borrow()
            .iter()
            .map(|drop| drop.publisher_key)
            .collect()
    }
}

impl DropRelay for FakeRelay {
    fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    fn fresh_publisher_key(&self) -> PublisherKey {
        let mut minted = self.keys_minted.borrow_mut();
        *minted += 1;
        // Distinct per call, which is all the module can be asked to prove: a real
        // implementation draws from the CSPRNG.
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&minted.to_be_bytes());
        PublisherKey::from_bytes(bytes)
    }

    fn publish(&self, request: PublishRequest<'_>) {
        self.published.borrow_mut().push(SentDrop {
            operation: request.operation,
            tag: request.recipient_tag,
            envelope: request.sealed_envelope.to_vec(),
            expires_at: request.expires_at_unix_secs,
            publisher_key: *request.publisher_key.as_bytes(),
        });
    }

    fn open_subscription(&self, tags: &[[u8; TAG_LEN]]) {
        self.subscriptions.borrow_mut().push(tags.to_vec());
    }

    fn close_subscription(&self) {
        *self.closes.borrow_mut() += 1;
    }
}

#[derive(Default)]
struct FakeSink {
    opened: RefCell<Vec<CourierEnvelope>>,
    accept_open: RefCell<bool>,
}

impl FakeSink {
    fn accepting() -> Self {
        let sink = Self::default();
        *sink.accept_open.borrow_mut() = true;
        sink
    }
}

impl DropSink for FakeSink {
    fn open_for_self(&self, envelope: &CourierEnvelope) -> bool {
        self.opened.borrow_mut().push(envelope.clone());
        *self.accept_open.borrow()
    }
}

// --- Helpers -----------------------------------------------------------

fn new_courier(connected: bool) -> BridgeCourier<FakeRelay, FakeSink> {
    let relay = FakeRelay::default();
    relay.set_connected(connected);
    let mut courier = BridgeCourier::new(relay, FakeSink::accepting());
    courier.set_bridge_enabled(true);
    courier
}

fn envelope_for(key: &[u8; 32], now: u64, body: u8) -> CourierEnvelope {
    CourierEnvelope::new(
        chatter_mesh::tlv::courier_envelope::recipient_tag(
            key,
            chatter_mesh::tlv::courier_envelope::epoch_day(now),
        ),
        millis(now + DAY),
        vec![body; 32],
        1,
        None,
    )
}

// --- The non-negotiable ------------------------------------------------

#[test]
fn every_publish_uses_a_fresh_throwaway_key() {
    let mut courier = new_courier(true);
    let key = [0x11u8; 32];
    let envelope = envelope_for(&key, NOW, 1);

    let first = courier.deposit("m1", &envelope, NOW);
    assert!(matches!(first.outcome, DepositOutcome::Publishing(_)));
    // The same envelope, deposited again under a different message ID: byte for
    // byte identical mail, and still a key nobody has seen.
    let second = courier.deposit("m2", &envelope, NOW);
    assert!(matches!(second.outcome, DepositOutcome::Publishing(_)));

    let keys = courier.relay().publisher_keys();
    assert_eq!(keys.len(), 2);
    assert_ne!(
        keys[0], keys[1],
        "a reused publisher key lets relays link every drop a user sends"
    );
    assert_eq!(
        *courier.relay().keys_minted.borrow(),
        2,
        "one key per publish"
    );
}

#[test]
fn a_retry_of_the_same_drop_also_gets_a_new_key() {
    let mut courier = new_courier(false);
    let key = [0x12u8; 32];
    let envelope = envelope_for(&key, NOW, 2);

    let deposited = courier.deposit("m1", &envelope, NOW);
    assert!(matches!(deposited.outcome, DepositOutcome::Queued(_)));
    assert_eq!(
        *courier.relay().keys_minted.borrow(),
        0,
        "queueing signs nothing"
    );

    courier.relay().set_connected(true);
    courier.flush_pending(NOW + 5);
    let operation = courier.relay().last_operation();
    // The relay rejected it; the router deposits the same message again later.
    assert!(
        !courier
            .resolve_publish(operation, false, NOW + 6)
            .expect("resolves")
            .accepted
    );
    let again = courier.deposit("m1", &envelope, NOW + 7);
    assert!(matches!(again.outcome, DepositOutcome::Publishing(_)));

    let keys = courier.relay().publisher_keys();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);
}

#[test]
fn a_publisher_key_does_not_print_itself() {
    let key = PublisherKey::from_bytes([0x7Au8; 32]);
    assert_eq!(format!("{key:?}"), "PublisherKey(<redacted>)");
    assert!(!format!("{key:?}").contains("122"));
}

// --- Adjacent-day tags -------------------------------------------------

#[test]
fn watches_the_adjacent_day_tags() {
    use chatter_mesh::tlv::courier_envelope::{epoch_day, recipient_tag};

    let mut courier = new_courier(true);
    let key = Some([0x21u8; 32]);
    courier.refresh(key, NOW);

    let day = epoch_day(NOW);
    let mine = courier.my_tags().to_vec();
    assert_eq!(mine.len(), 3);
    for candidate in [day - 1, day, day + 1] {
        assert!(
            mine.contains(&recipient_tag(&[0x21u8; 32], candidate)),
            "day {candidate} must be watched"
        );
    }
}

#[test]
fn a_drop_published_before_utc_midnight_is_still_findable_after_it() {
    use chatter_mesh::tlv::courier_envelope::{epoch_day, recipient_tag};

    let key = [0x22u8; 32];
    // One second before a UTC midnight, and one second after it.
    let before = (NOW / DAY + 1) * DAY - 1;
    let after = before + 2;
    assert_eq!(epoch_day(after), epoch_day(before) + 1, "the day must roll");

    let sender_tag = recipient_tag(&key, epoch_day(before));
    let mut courier = new_courier(true);
    let local = Some(key);

    courier.refresh(local, before);
    assert!(courier.my_tags().contains(&sender_tag));

    // After the rollover the recipient keeps yesterday's tag in the watch set,
    // which is the only reason a drop sealed a second before midnight is ever
    // found.
    courier.refresh(local, after);
    assert!(
        courier.my_tags().contains(&sender_tag),
        "yesterday's tag must survive the rollover"
    );
    assert!(
        courier
            .my_tags()
            .contains(&recipient_tag(&key, epoch_day(after)))
    );

    let envelope = CourierEnvelope::new(sender_tag, millis(after + DAY), vec![9; 16], 1, None);
    let encoded = envelope.encode().expect("encodes");
    let event = DropEvent {
        event_id: "e-rollover",
        tag: sender_tag,
        envelope_bytes: &encoded,
    };
    assert_eq!(
        courier.handle_drop(&event, after),
        DropDisposition::OpenedForSelf
    );
}

#[test]
fn the_subscription_is_rebuilt_only_when_the_tag_set_changes() {
    let mut courier = new_courier(true);
    let key = Some([0x23u8; 32]);
    courier.refresh(key, NOW);
    courier.refresh(key, NOW + 60);
    assert_eq!(courier.relay().subscriptions.borrow().len(), 1);

    // A day later the tags have rotated, so the subscription must be reopened.
    courier.refresh(key, NOW + DAY);
    assert_eq!(courier.relay().subscriptions.borrow().len(), 2);
}

// --- The pending queue -------------------------------------------------

#[test]
fn the_pending_queue_drops_the_oldest_at_its_exact_bound() {
    let mut courier = new_courier(false);
    let key = [0x31u8; 32];
    let mut first_operation = None;

    for index in 0..MAX_PENDING_DROPS {
        let deposited = courier.deposit(
            &format!("m{index}"),
            &envelope_for(&key, NOW, index as u8),
            NOW,
        );
        assert!(
            deposited.abandoned.is_empty(),
            "nothing evicted below the bound"
        );
        if index == 0 {
            first_operation = match deposited.outcome {
                DepositOutcome::Queued(op) => Some(op),
                other => panic!("expected a queued drop, got {other:?}"),
            };
        }
    }
    assert_eq!(courier.pending_len(), MAX_PENDING_DROPS);

    // One past the bound: the oldest goes, the queue stays at its cap.
    let overflow = courier.deposit("m-overflow", &envelope_for(&key, NOW, 99), NOW);
    assert_eq!(courier.pending_len(), MAX_PENDING_DROPS);
    assert_eq!(
        overflow.abandoned,
        vec![AbandonedDrop {
            operation: first_operation.expect("first operation"),
            message_id: "m0".to_string(),
        }]
    );

    // And the evicted message is the one no longer flushed.
    courier.relay().set_connected(true);
    courier.flush_pending(NOW + 1);
    assert_eq!(courier.relay().publish_count(), MAX_PENDING_DROPS);
    assert_eq!(courier.pending_len(), 0);
}

#[test]
fn a_deposit_is_idempotent_per_message_id() {
    let mut courier = new_courier(true);
    let key = [0x32u8; 32];
    let envelope = envelope_for(&key, NOW, 1);

    let first = courier.deposit("m1", &envelope, NOW);
    let DepositOutcome::Publishing(operation) = first.outcome else {
        panic!("expected a publish");
    };
    assert_eq!(
        courier.deposit("m1", &envelope, NOW).outcome,
        DepositOutcome::Refused(DepositRefusal::AlreadyInFlight)
    );

    courier
        .resolve_publish(operation, true, NOW)
        .expect("resolves");
    assert_eq!(
        courier.deposit("m1", &envelope, NOW).outcome,
        DepositOutcome::Refused(DepositRefusal::AlreadyPublished)
    );
    assert_eq!(courier.relay().publish_count(), 1);
}

#[test]
fn a_rejected_relay_result_leaves_the_message_depositable_again() {
    let mut courier = new_courier(true);
    let envelope = envelope_for(&[0x33u8; 32], NOW, 1);
    let DepositOutcome::Publishing(operation) = courier.deposit("m1", &envelope, NOW).outcome
    else {
        panic!("expected a publish");
    };
    courier
        .resolve_publish(operation, false, NOW)
        .expect("resolves");
    assert!(matches!(
        courier.deposit("m1", &envelope, NOW + 1).outcome,
        DepositOutcome::Publishing(_)
    ));
}

#[test]
fn a_stale_relay_callback_cannot_complete_a_newer_attempt() {
    let mut courier = new_courier(true);
    let envelope = envelope_for(&[0x34u8; 32], NOW, 1);
    let DepositOutcome::Publishing(operation) = courier.deposit("m1", &envelope, NOW).outcome
    else {
        panic!("expected a publish");
    };
    assert!(courier.resolve_publish(operation, true, NOW).is_some());
    // The same callback arriving twice, and one for an operation that was wiped.
    assert!(courier.resolve_publish(operation, true, NOW).is_none());
    assert!(
        courier
            .resolve_publish(DropOperationId(9_999), true, NOW)
            .is_none()
    );
}

#[test]
fn an_oversized_envelope_is_suppressed_rather_than_retried() {
    let mut courier = new_courier(true);
    let envelope = CourierEnvelope::new(
        [0x35u8; TAG_LEN],
        millis(NOW + DAY),
        vec![7u8; MAX_DROP_ENVELOPE_BYTES + 1],
        1,
        None,
    );
    assert_eq!(
        courier.deposit("m-big", &envelope, NOW).outcome,
        DepositOutcome::Refused(DepositRefusal::Undeliverable)
    );
    // Suppressed for the process, and never recorded as if a relay took it.
    assert_eq!(
        courier.deposit("m-big", &envelope, NOW).outcome,
        DepositOutcome::Refused(DepositRefusal::Undeliverable)
    );
    assert_eq!(courier.relay().publish_count(), 0);
    assert!(courier.dedup_snapshot().published_drop_keys.is_empty());
}

#[test]
fn an_expired_envelope_is_never_published() {
    let mut courier = new_courier(true);
    let expired = CourierEnvelope::new([0x36u8; TAG_LEN], millis(NOW), vec![1; 8], 1, None);
    assert_eq!(
        courier.deposit("m-old", &expired, NOW).outcome,
        DepositOutcome::Refused(DepositRefusal::Expired)
    );
    assert_eq!(courier.relay().publish_count(), 0);
}

#[test]
fn nothing_is_published_while_the_bridge_is_off() {
    let mut courier = new_courier(true);
    courier.set_bridge_enabled(false);
    assert_eq!(
        courier
            .deposit("m1", &envelope_for(&[0x37u8; 32], NOW, 1), NOW)
            .outcome,
        DepositOutcome::Refused(DepositRefusal::BridgeDisabled)
    );
    assert_eq!(courier.relay().publish_count(), 0);
}

#[test]
fn disabling_the_bridge_cancels_queued_and_in_flight_drops() {
    let mut courier = new_courier(true);
    let key = [0x38u8; 32];
    let DepositOutcome::Publishing(in_flight) = courier
        .deposit("m1", &envelope_for(&key, NOW, 1), NOW)
        .outcome
    else {
        panic!("expected a publish");
    };
    courier.relay().set_connected(false);
    let DepositOutcome::Queued(queued) = courier
        .deposit("m2", &envelope_for(&key, NOW, 2), NOW)
        .outcome
    else {
        panic!("expected a queue");
    };

    courier.set_bridge_enabled(false);
    let abandoned = courier.refresh(Some(key), NOW);
    let operations: Vec<DropOperationId> = abandoned.iter().map(|drop| drop.operation).collect();
    assert!(operations.contains(&in_flight));
    assert!(operations.contains(&queued));
    // A late relay callback for a cancelled operation finds nothing.
    assert!(courier.resolve_publish(in_flight, true, NOW).is_none());
    assert_eq!(courier.pending_len(), 0);
}

#[test]
fn the_drop_carries_the_envelope_tag_and_a_nip40_expiry() {
    let mut courier = new_courier(true);
    let key = [0x39u8; 32];
    let envelope = envelope_for(&key, NOW, 5);
    courier.deposit("m1", &envelope, NOW);

    let published = courier.relay().published.borrow();
    let drop = published.first().expect("a drop");
    assert_eq!(drop.tag, envelope.recipient_tag);
    assert_eq!(drop.envelope, envelope.encode().expect("encodes"));
    assert_eq!(drop.expires_at, envelope.expiry_ms / 1_000);
    assert!(drop.expires_at > NOW, "NIP-40 must be in the future");
}

// --- Inbound drops -----------------------------------------------------

#[test]
fn a_drop_for_an_unknown_tag_is_ignored_rather_than_opened() {
    let mut courier = new_courier(true);
    courier.refresh(Some([0x61u8; 32]), NOW);

    // Someone else's mail, addressed to a tag we do not watch.
    let envelope = envelope_for(&[0x62u8; 32], NOW, 9);
    let encoded = envelope.encode().expect("encodes");
    let disposition = courier.handle_drop(
        &DropEvent {
            event_id: "e-stranger",
            tag: envelope.recipient_tag,
            envelope_bytes: &encoded,
        },
        NOW,
    );
    assert_eq!(
        disposition,
        DropDisposition::Ignored(IgnoreReason::NotWatched)
    );
    assert!(
        courier.sink().opened.borrow().is_empty(),
        "an unwatched tag must not reach the seal at all"
    );
    assert!(courier.dedup_snapshot().seen_drop_event_ids.is_empty());
}

#[test]
fn a_mislabelled_drop_cannot_ride_someone_elses_subscription() {
    let mut courier = new_courier(true);
    let key = [0x63u8; 32];
    courier.refresh(Some(key), NOW);
    let our_tag = courier.my_tags()[1];

    // The event claims our tag; the envelope inside is addressed elsewhere.
    let envelope = envelope_for(&[0x64u8; 32], NOW, 10);
    let encoded = envelope.encode().expect("encodes");
    let disposition = courier.handle_drop(
        &DropEvent {
            event_id: "e-mislabelled",
            tag: our_tag,
            envelope_bytes: &encoded,
        },
        NOW,
    );
    assert_eq!(
        disposition,
        DropDisposition::Ignored(IgnoreReason::Mislabelled)
    );
    assert!(courier.sink().opened.borrow().is_empty());
}

#[test]
fn the_same_drop_from_two_relays_is_opened_once() {
    let mut courier = new_courier(true);
    let key = [0x65u8; 32];
    courier.refresh(Some(key), NOW);
    let envelope = envelope_for(&key, NOW, 11);
    let encoded = envelope.encode().expect("encodes");
    let event = DropEvent {
        event_id: "e-dup",
        tag: envelope.recipient_tag,
        envelope_bytes: &encoded,
    };

    assert_eq!(
        courier.handle_drop(&event, NOW),
        DropDisposition::OpenedForSelf
    );
    assert_eq!(
        courier.handle_drop(&event, NOW),
        DropDisposition::Ignored(IgnoreReason::AlreadyHandled)
    );
    assert_eq!(courier.sink().opened.borrow().len(), 1);
}

#[test]
fn a_transient_sink_failure_leaves_the_drop_retryable() {
    let mut courier = new_courier(true);
    let key = [0x66u8; 32];
    courier.refresh(Some(key), NOW);
    *courier.sink().accept_open.borrow_mut() = false;

    let envelope = envelope_for(&key, NOW, 12);
    let encoded = envelope.encode().expect("encodes");
    let event = DropEvent {
        event_id: "e-retry",
        tag: envelope.recipient_tag,
        envelope_bytes: &encoded,
    };
    assert_eq!(courier.handle_drop(&event, NOW), DropDisposition::Retryable);
    assert!(courier.dedup_snapshot().seen_drop_event_ids.is_empty());

    *courier.sink().accept_open.borrow_mut() = true;
    assert_eq!(
        courier.handle_drop(&event, NOW),
        DropDisposition::OpenedForSelf
    );
}

#[test]
fn malformed_and_expired_drops_go_no_further() {
    let mut courier = new_courier(true);
    let key = [0x67u8; 32];
    courier.refresh(Some(key), NOW);

    assert_eq!(
        courier.handle_drop(
            &DropEvent {
                event_id: "e-garbage",
                tag: [0u8; TAG_LEN],
                envelope_bytes: &[0xFF, 0x00],
            },
            NOW,
        ),
        DropDisposition::Ignored(IgnoreReason::Malformed)
    );

    assert_eq!(
        courier.handle_drop(
            &DropEvent {
                event_id: "e-huge",
                tag: [0u8; TAG_LEN],
                envelope_bytes: &vec![0u8; MAX_DROP_ENVELOPE_BYTES + 1],
            },
            NOW,
        ),
        DropDisposition::Ignored(IgnoreReason::Malformed)
    );

    let expired = CourierEnvelope::new(courier.my_tags()[1], millis(NOW - 1), vec![1; 8], 1, None);
    let encoded = expired.encode().expect("encodes");
    assert_eq!(
        courier.handle_drop(
            &DropEvent {
                event_id: "e-expired",
                tag: expired.recipient_tag,
                envelope_bytes: &encoded,
            },
            NOW,
        ),
        DropDisposition::Ignored(IgnoreReason::Expired)
    );
    assert!(courier.sink().opened.borrow().is_empty());
}

#[test]
fn drops_are_ignored_while_the_bridge_is_off() {
    let mut courier = new_courier(true);
    let key = [0x68u8; 32];
    courier.refresh(Some(key), NOW);
    courier.set_bridge_enabled(false);

    let envelope = envelope_for(&key, NOW, 13);
    let encoded = envelope.encode().expect("encodes");
    assert_eq!(
        courier.handle_drop(
            &DropEvent {
                event_id: "e-off",
                tag: envelope.recipient_tag,
                envelope_bytes: &encoded,
            },
            NOW,
        ),
        DropDisposition::Ignored(IgnoreReason::BridgeDisabled)
    );
}

// --- Refresh scheduling ------------------------------------------------

#[test]
fn the_refresh_falls_due_every_half_hour() {
    let mut courier = new_courier(true);
    courier.refresh(Some([0x71u8; 32]), NOW);
    assert_eq!(courier.next_refresh_due(), NOW + REFRESH_INTERVAL_SECS);
}

#[test]
fn a_disconnected_refresh_closes_the_subscription_and_publishes_nothing() {
    let mut courier = new_courier(true);
    let key = Some([0x73u8; 32]);
    courier.refresh(key, NOW);
    assert!(courier.is_subscribed());

    courier.relay().set_connected(false);
    courier.refresh(key, NOW + 1);
    assert!(!courier.is_subscribed());
    assert_eq!(*courier.relay().closes.borrow(), 1);
    assert_eq!(courier.relay().publish_count(), 0);
}

#[test]
fn with_no_key_there_is_nothing_to_subscribe_to() {
    let mut courier = new_courier(true);
    courier.refresh(None, NOW);
    assert!(!courier.is_subscribed());
    assert!(courier.relay().subscriptions.borrow().is_empty());
}

// --- Dedup record ------------------------------------------------------

#[test]
fn the_dedup_record_survives_a_restart() {
    let mut courier = new_courier(true);
    let key = [0x81u8; 32];
    let envelope = envelope_for(&key, NOW, 14);
    let DepositOutcome::Publishing(operation) = courier.deposit("m1", &envelope, NOW).outcome
    else {
        panic!("expected a publish");
    };
    courier
        .resolve_publish(operation, true, NOW)
        .expect("resolves");
    let snapshot = courier.dedup_snapshot();
    assert_eq!(snapshot.published_drop_keys.len(), 1);

    // A fresh process reloads the record and refuses to republish the same mail,
    // which is what stops a relaunch from re-flooding the relays.
    let mut restarted = new_courier(true);
    restarted.restore_dedup(&snapshot, NOW + 60);
    assert_eq!(
        restarted.deposit("m1", &envelope, NOW + 60).outcome,
        DepositOutcome::Refused(DepositRefusal::AlreadyPublished)
    );

    // A record older than the drop window is not restored: the relay has dropped
    // the event by then, so republishing is correct again.
    let mut much_later = new_courier(true);
    much_later.restore_dedup(&snapshot, NOW + TRACKED_ID_LIFETIME_SECS + 1);
    assert!(matches!(
        much_later
            .deposit(
                "m1",
                &envelope_for(&key, NOW + TRACKED_ID_LIFETIME_SECS + 1, 15),
                NOW + TRACKED_ID_LIFETIME_SECS + 1
            )
            .outcome,
        DepositOutcome::Publishing(_)
    ));
}

#[test]
fn a_wipe_forgets_queued_drops_and_the_dedup_record() {
    let mut courier = new_courier(true);
    let key = [0x82u8; 32];
    let DepositOutcome::Publishing(operation) = courier
        .deposit("m1", &envelope_for(&key, NOW, 16), NOW)
        .outcome
    else {
        panic!("expected a publish");
    };
    courier
        .resolve_publish(operation, true, NOW)
        .expect("resolves");
    courier.relay().set_connected(false);
    courier.deposit("m2", &envelope_for(&key, NOW, 17), NOW);

    let abandoned = courier.wipe();
    assert_eq!(abandoned.len(), 1, "the queued drop is abandoned");
    assert_eq!(courier.dedup_snapshot(), DedupSnapshot::default());
    assert_eq!(courier.pending_len(), 0);
    assert!(courier.my_tags().is_empty());
}

// --- The expiring set ---------------------------------------------------

#[test]
fn the_id_set_ages_out_and_stays_bounded() {
    let mut set = ExpiringIdSet::new(3, 100);
    set.insert("a", 0);
    set.insert("b", 10);
    assert!(set.contains("a", 50));
    assert!(
        !set.contains("a", 100),
        "exactly at the lifetime is expired"
    );

    set.insert("c", 20);
    set.insert("d", 30);
    assert_eq!(set.len(), 3);
    assert!(
        !set.contains("a", 30),
        "the oldest was evicted at the bound"
    );
    assert!(set.contains("d", 30));

    // Re-inserting refreshes rather than duplicating.
    set.insert("d", 40);
    assert_eq!(set.len(), 3);
    assert!(set.contains("d", 139));
}
