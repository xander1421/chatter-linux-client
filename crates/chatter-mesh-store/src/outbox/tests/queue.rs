use super::*;

// ------------------------------------------------------------------ basics

#[test]
fn enqueue_retains_in_order() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 100)).is_none());
    assert!(store.enqueue(&p, message("m2", 200)).is_none());

    let queue = store.queue(&p);
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.first().map(|m| m.message_id.as_str()), Some("m1"));
    assert_eq!(queue.get(1).map(|m| m.message_id.as_str()), Some("m2"));
    assert_eq!(store.len(), 2);
}

#[test]
fn queued_finds_only_that_peers_mail() {
    let (_storage, mut store) = new_store();
    let a = peer("aa");
    let b = peer("bb");
    assert!(store.enqueue(&a, message("m1", 100)).is_none());

    assert!(store.queued(&a, "m1").is_some());
    assert!(store.queued(&b, "m1").is_none());
    assert!(store.queued(&a, "nope").is_none());
}

// --------------------------------------------------------------- TTL edges

#[test]
fn ttl_boundary_is_exact() {
    let sent = 1_000_000u64;
    for (age, expired) in [
        (MESSAGE_TTL_SECS - 1, false),
        (MESSAGE_TTL_SECS, false),
        (MESSAGE_TTL_SECS + 1, true),
    ] {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store.enqueue(&p, message("m1", sent)).is_none());

        let dropped = store.cleanup_expired(sent + age);
        assert_eq!(
            !dropped.is_empty(),
            expired,
            "age {age}s should{} expire",
            if expired { "" } else { " not" }
        );
        assert_eq!(store.queue(&p).is_empty(), expired);
    }
}

#[test]
fn expiry_reports_the_reason_and_the_peer() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 0)).is_none());

    let dropped = store.cleanup_expired(MESSAGE_TTL_SECS + 1);
    assert_eq!(dropped.len(), 1);
    let drop = dropped.first().expect("one drop");
    assert_eq!(drop.message_id, "m1");
    assert_eq!(drop.peer, p);
    assert_eq!(drop.reason, DropReason::Expired);
}

#[test]
fn a_backwards_clock_does_not_expire_everything() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 5_000)).is_none());

    assert!(store.cleanup_expired(1_000).is_empty());
    assert_eq!(store.queue(&p).len(), 1);
}

#[test]
fn flush_drops_expired_before_looking_at_the_link() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("old", 0)).is_none());
    assert!(store
        .enqueue(&p, message("new", MESSAGE_TTL_SECS))
        .is_none());

    let plan = store.flush(&p, LinkState::Secure, MESSAGE_TTL_SECS + 1);
    assert_eq!(plan.dropped.len(), 1);
    assert_eq!(
        plan.dropped.first().map(|d| d.message_id.as_str()),
        Some("old")
    );
    assert_eq!(plan.send.len(), 1);
    assert_eq!(
        plan.send.first().map(|m| m.message_id.as_str()),
        Some("new")
    );
}

// ---------------------------------------------------------- per-peer cap

#[test]
fn per_peer_cap_admits_exactly_one_hundred() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    for i in 0..MAX_MESSAGES_PER_PEER {
        let id = format!("m{i}");
        assert!(
            store.enqueue(&p, message(&id, i as u64)).is_none(),
            "message {i} must not evict anything"
        );
    }
    assert_eq!(store.queue(&p).len(), MAX_MESSAGES_PER_PEER);
}

#[test]
fn message_one_hundred_and_one_evicts_the_oldest_and_reports_it() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    for i in 0..MAX_MESSAGES_PER_PEER {
        let id = format!("m{i}");
        assert!(store.enqueue(&p, message(&id, i as u64)).is_none());
    }

    let evicted = store
        .enqueue(&p, message("overflow", 999_999))
        .expect("the 101st message must evict the oldest");
    assert_eq!(evicted.message_id, "m0");
    assert_eq!(evicted.peer, p);
    assert_eq!(evicted.reason, DropReason::QueueOverflow);
    assert_eq!(store.queue(&p).len(), MAX_MESSAGES_PER_PEER);
    assert!(store.queued(&p, "m0").is_none());
    assert!(store.queued(&p, "overflow").is_some());
}

// ------------------------------------------------------------ attempt cap

#[test]
fn attempt_cap_boundary_is_exact() {
    // 7 attempts spent: one more send is allowed, and it becomes the 8th.
    // 8 and 9: the cap is reached and the message is given up on.
    for (spent, sends) in [
        (MAX_SEND_ATTEMPTS - 1, true),
        (MAX_SEND_ATTEMPTS, false),
        (MAX_SEND_ATTEMPTS + 1, false),
    ] {
        let (_storage, mut store) = new_store();
        let p = peer("aa");
        assert!(store
            .enqueue(&p, message("m1", 100).with_send_attempts(spent))
            .is_none());

        let plan = store.flush(&p, LinkState::Reachable, 200);
        if sends {
            assert_eq!(plan.send.len(), 1, "{spent} attempts should still send");
            assert!(plan.dropped.is_empty());
            assert_eq!(
                store.queued(&p, "m1").map(|m| m.send_attempts),
                Some(MAX_SEND_ATTEMPTS),
                "the send must be charged"
            );
        } else {
            assert!(plan.send.is_empty(), "{spent} attempts must not send");
            assert_eq!(plan.dropped.len(), 1);
            assert_eq!(
                plan.dropped.first().map(|d| d.reason),
                Some(DropReason::AttemptsExhausted)
            );
            assert!(store.queue(&p).is_empty());
        }
    }
}

#[test]
fn reachable_flushes_walk_the_cap_to_exhaustion() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 0)).is_none());

    for attempt in 0..MAX_SEND_ATTEMPTS {
        let plan = store.flush(&p, LinkState::Reachable, 100);
        assert_eq!(plan.send.len(), 1, "attempt {attempt} should send");
        assert!(plan.dropped.is_empty());
    }
    let plan = store.flush(&p, LinkState::Reachable, 100);
    assert!(plan.send.is_empty());
    assert_eq!(plan.dropped.len(), 1);
    assert_eq!(
        plan.dropped.first().map(|d| d.reason),
        Some(DropReason::AttemptsExhausted)
    );
}

// --------------------------------------------------- secure vs insecure

#[test]
fn a_secure_flush_sends_and_stops_retaining() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 100)).is_none());

    let plan = store.flush(&p, LinkState::Secure, 200);
    assert_eq!(plan.send.len(), 1);
    assert!(plan.dropped.is_empty());
    assert!(
        store.queue(&p).is_empty(),
        "a secure link is the one signal strong enough to stop retaining"
    );
}

#[test]
fn an_insecure_flush_sends_retains_and_charges_nothing() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 100)).is_none());

    let plan = store.flush(&p, LinkState::Insecure, 200);
    assert_eq!(plan.send.len(), 1);
    assert!(plan.dropped.is_empty());
    assert_eq!(store.queued(&p, "m1").map(|m| m.send_attempts), Some(0));
}

#[test]
fn reconnect_flapping_without_a_session_never_burns_the_message() {
    // The whole point of the insecure rule: a handshake that keeps stalling
    // must not consume the retention that exists to survive it. Far more
    // flushes than the attempt cap, and the copy is still there.
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 0)).is_none());

    for _ in 0..(MAX_SEND_ATTEMPTS * 5) {
        let plan = store.flush(&p, LinkState::Insecure, 100);
        assert_eq!(plan.send.len(), 1);
        assert!(plan.dropped.is_empty());
    }
    assert_eq!(store.queued(&p, "m1").map(|m| m.send_attempts), Some(0));

    // Retention is still bounded — by the TTL, not by the attempt cap.
    let dropped = store.cleanup_expired(MESSAGE_TTL_SECS + 1);
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped.first().map(|d| d.reason), Some(DropReason::Expired));
}

#[test]
fn insecure_and_reachable_differ_only_in_the_attempt_charge() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 0)).is_none());
    assert!(store.enqueue(&peer("bb"), message("m2", 0)).is_none());

    let insecure = store.flush(&p, LinkState::Insecure, 100);
    let reachable = store.flush(&peer("bb"), LinkState::Reachable, 100);
    assert_eq!(insecure.send.len(), reachable.send.len());
    assert_eq!(store.queued(&p, "m1").map(|m| m.send_attempts), Some(0));
    assert_eq!(
        store.queued(&peer("bb"), "m2").map(|m| m.send_attempts),
        Some(1)
    );
}

#[test]
fn an_unreachable_flush_changes_nothing() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store
        .enqueue(&p, message("m1", 100).with_send_attempts(3))
        .is_none());

    let plan = store.flush(&p, LinkState::Unreachable, 200);
    assert!(plan.is_empty());
    assert_eq!(store.queued(&p, "m1").map(|m| m.send_attempts), Some(3));
}

#[test]
fn flushing_an_unknown_peer_is_a_no_op() {
    let (_storage, mut store) = new_store();
    let plan = store.flush(&peer("nobody"), LinkState::Secure, 100);
    assert!(plan.is_empty());
}
