use super::*;

// -------------------------------------------------------- courier keys

#[test]
fn re_enqueue_preserves_deposited_courier_keys() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 100)).is_none());
    store.record_courier_deposit(&p, "m1", &[key(1), key(2)]);

    // The router re-sends the same ID with a fresh attempt count.
    assert!(store
        .enqueue(&p, message("m1", 150).with_send_attempts(1))
        .is_none());

    let queued = store.queued(&p, "m1").expect("still queued");
    assert_eq!(
        queued.deposited_courier_keys.len(),
        2,
        "the replacement must not re-burn courier slots already spent"
    );
    assert!(queued.is_carried_by(&key(1)));
    assert!(queued.is_carried_by(&key(2)));
    assert_eq!(queued.send_attempts, 1);
    assert_eq!(queued.timestamp, 150);
    assert_eq!(
        store.queue(&p).len(),
        1,
        "the ID is replaced, not duplicated"
    );
}

#[test]
fn re_enqueue_moves_the_message_to_the_back() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 100)).is_none());
    assert!(store.enqueue(&p, message("m2", 200)).is_none());
    assert!(store.enqueue(&p, message("m1", 300)).is_none());

    let ids: Vec<&str> = store
        .queue(&p)
        .iter()
        .map(|m| m.message_id.as_str())
        .collect();
    assert_eq!(ids, vec!["m2", "m1"]);
}

#[test]
fn courier_deposits_union_and_track_remaining_slots() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 100)).is_none());
    assert_eq!(
        store
            .queued(&p, "m1")
            .map(QueuedMessage::remaining_courier_slots),
        Some(MAX_COURIERS_PER_MESSAGE)
    );

    store.record_courier_deposit(&p, "m1", &[key(1)]);
    store.record_courier_deposit(&p, "m1", &[key(1), key(2)]);
    let queued = store.queued(&p, "m1").expect("queued");
    assert_eq!(queued.deposited_courier_keys.len(), 2);
    assert_eq!(
        queued.remaining_courier_slots(),
        MAX_COURIERS_PER_MESSAGE - 2
    );

    store.record_courier_deposit(&p, "m1", &[key(3)]);
    let queued = store.queued(&p, "m1").expect("queued");
    assert_eq!(queued.remaining_courier_slots(), 0);
    assert!(!queued.is_carried_by(&key(9)));
}

#[test]
fn a_deposit_for_acked_mail_is_ignored() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 100)).is_none());
    assert!(
        store
            .mark_acked(&peer("aa"), "m1", AckKind::Delivered)
            .cleared
    );

    store.record_courier_deposit(&p, "m1", &[key(1)]);
    assert!(store.queued(&p, "m1").is_none());
}

// ------------------------------------------------------- recovery merge

#[test]
fn recovery_merges_rather_than_replaces() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    // Accepted during the locked window.
    assert!(store.enqueue(&p, message("local", 200)).is_none());

    let mut durable = Snapshot::new();
    durable.insert(p.clone(), vec![message("durable", 100)]);
    assert!(store.merge_recovered(durable).is_empty());

    let ids: Vec<&str> = store
        .queue(&p)
        .iter()
        .map(|m| m.message_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["durable", "local"],
        "both survive, sorted by timestamp"
    );
}

#[test]
fn recovery_takes_the_higher_attempt_count_and_unions_courier_keys() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    let mut local = message("m1", 100).with_send_attempts(5);
    local.deposited_courier_keys.insert(key(1));
    assert!(store.enqueue(&p, local).is_none());

    let mut recovered = message("m1", 100).with_send_attempts(2);
    recovered.deposited_courier_keys.insert(key(2));
    let mut durable = Snapshot::new();
    durable.insert(p.clone(), vec![recovered]);
    assert!(store.merge_recovered(durable).is_empty());

    let merged = store.queued(&p, "m1").expect("merged");
    assert_eq!(merged.send_attempts, 5, "attempts must not regress");
    assert_eq!(merged.deposited_courier_keys.len(), 2);
    assert!(merged.is_carried_by(&key(1)));
    assert!(merged.is_carried_by(&key(2)));
}

#[test]
fn recovery_lets_a_higher_recovered_attempt_count_win() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store
        .enqueue(&p, message("m1", 100).with_send_attempts(1))
        .is_none());

    let mut durable = Snapshot::new();
    durable.insert(p.clone(), vec![message("m1", 100).with_send_attempts(6)]);
    assert!(store.merge_recovered(durable).is_empty());
    assert_eq!(store.queued(&p, "m1").map(|m| m.send_attempts), Some(6));
}

#[test]
fn recovery_sorts_by_timestamp_before_applying_the_cap() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    // Newest in memory, oldest on disk: an unsorted merge would evict the
    // wrong half.
    for i in 0..MAX_MESSAGES_PER_PEER {
        let id = format!("new{i}");
        assert!(store.enqueue(&p, message(&id, 10_000 + i as u64)).is_none());
    }
    let durable_queue: Vec<QueuedMessage> = (0..5)
        .map(|i| message(&format!("old{i}"), i as u64))
        .collect();
    let mut durable = Snapshot::new();
    durable.insert(p.clone(), durable_queue);

    let dropped = store.merge_recovered(durable);
    assert_eq!(dropped.len(), 5, "the overflow is the five oldest");
    let dropped_ids: Vec<&str> = dropped.iter().map(|d| d.message_id.as_str()).collect();
    assert_eq!(dropped_ids, vec!["old0", "old1", "old2", "old3", "old4"]);
    assert!(
        dropped
            .iter()
            .all(|d| d.reason == DropReason::QueueOverflow),
        "the UI has to be able to fail these"
    );
    assert_eq!(store.queue(&p).len(), MAX_MESSAGES_PER_PEER);
    assert!(store.queued(&p, "new0").is_some());
}

#[test]
fn recovery_does_not_resurrect_acked_mail() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    // The ack landed while the durable queue was still unreadable.
    let outcome = store.mark_acked(&peer("aa"), "m1", AckKind::Read);
    assert!(!outcome.cleared);

    let mut durable = Snapshot::new();
    durable.insert(p.clone(), vec![message("m1", 100), message("m2", 100)]);
    assert!(store.merge_recovered(durable).is_empty());

    assert!(store.queued(&p, "m1").is_none(), "the ack is a tombstone");
    assert!(store.queued(&p, "m2").is_some());
}

#[test]
fn recovery_leaves_untouched_peers_alone() {
    let (_storage, mut store) = new_store();
    assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());

    let mut durable = Snapshot::new();
    durable.insert(peer("bb"), vec![message("m2", 100)]);
    assert!(store.merge_recovered(durable).is_empty());

    assert_eq!(store.queue(&peer("aa")).len(), 1);
    assert_eq!(store.queue(&peer("bb")).len(), 1);
}
