use super::*;

// ------------------------------------------------------------------- acks

#[test]
fn a_delivery_ack_clears_the_message() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 100)).is_none());

    let outcome = store.mark_acked(&p, "m1", AckKind::Delivered);
    assert!(outcome.cleared);
    assert_eq!(outcome.strongest, AckKind::Delivered);
    assert!(
        store.is_empty(),
        "the peer entry goes when its queue empties"
    );
}

#[test]
fn a_read_ack_clears_the_message() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 100)).is_none());

    let outcome = store.mark_acked(&p, "m1", AckKind::Read);
    assert!(outcome.cleared);
    assert_eq!(outcome.strongest, AckKind::Read);
    assert!(store.queue(&p).is_empty());
}

#[test]
fn a_late_delivery_ack_never_downgrades_a_read_ack() {
    let (_storage, mut store) = new_store();
    let p = peer("aa");
    assert!(store.enqueue(&p, message("m1", 100)).is_none());

    assert_eq!(
        store.mark_acked(&p, "m1", AckKind::Read).strongest,
        AckKind::Read
    );
    let late = store.mark_acked(&p, "m1", AckKind::Delivered);
    assert!(!late.cleared, "there was nothing left to clear");
    assert_eq!(
        late.strongest,
        AckKind::Read,
        "a delivery ack must never walk a read ack backwards"
    );
    assert_eq!(store.ack_state("m1"), Some(AckKind::Read));
}

#[test]
fn a_read_ack_upgrades_an_earlier_delivery_ack() {
    let (_storage, mut store) = new_store();
    assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());

    assert_eq!(
        store
            .mark_acked(&peer("aa"), "m1", AckKind::Delivered)
            .strongest,
        AckKind::Delivered
    );
    assert_eq!(
        store.mark_acked(&peer("aa"), "m1", AckKind::Read).strongest,
        AckKind::Read
    );
}

#[test]
fn an_ack_for_unseen_mail_is_still_remembered() {
    // The ack can arrive while the durable queue is unreadable. Recording it
    // is what stops the recovery merge resurrecting the message.
    let (_storage, mut store) = new_store();
    let outcome = store.mark_acked(&peer("aa"), "ghost", AckKind::Delivered);
    assert!(!outcome.cleared);
    assert_eq!(store.ack_state("ghost"), Some(AckKind::Delivered));
}

#[test]
fn an_ack_clears_only_the_acking_peers_copy() {
    let (_storage, mut store) = new_store();
    assert!(store.enqueue(&peer("aa"), message("m1", 100)).is_none());
    assert!(store.enqueue(&peer("bb"), message("m2", 100)).is_none());

    assert!(
        store
            .mark_acked(&peer("bb"), "m2", AckKind::Delivered)
            .cleared
    );
    assert_eq!(store.queue(&peer("aa")).len(), 1);
    assert!(store.queue(&peer("bb")).is_empty());
}

/// §100. This test was `an_ack_clears_the_message_whichever_peer_holds_it`,
/// and the implementation matched: mark_acked walked EVERY peer's queue and
/// dropped any message with that ID.
///
/// Acks arrive Noise-decrypted, so a peer can only ack as itself — but it
/// could still cancel mail addressed to somebody else, and the sender was
/// told "delivered" for a message that was then never sent. The same ID
/// reaching two peers is not exotic either: a fan-out enqueues one message
/// id per recipient.
#[test]
fn one_peers_ack_cannot_cancel_another_peers_mail() {
    let (_storage, mut store) = new_store();
    let recipient = peer("aa");
    let bystander = peer("bb");
    assert!(store.enqueue(&recipient, message("m1", 100)).is_none());
    assert!(store.enqueue(&bystander, message("m1", 100)).is_none());

    let outcome = store.mark_acked(&bystander, "m1", AckKind::Delivered);
    assert!(outcome.cleared, "the acking peer's own copy goes");
    assert!(store.queue(&bystander).is_empty());
    assert_eq!(
        store.queue(&recipient).len(),
        1,
        "the recipient has not acknowledged anything and must still be owed the message"
    );
}

#[test]
fn ack_memory_stays_bounded() {
    let (_storage, mut store) = new_store();
    for i in 0..(ACK_MEMORY_LIMIT + 10) {
        let _ = store.mark_acked(&peer("aa"), &format!("m{i}"), AckKind::Delivered);
    }
    assert_eq!(store.ack_state("m0"), None, "the oldest acks are forgotten");
    assert_eq!(
        store.ack_state(&format!("m{}", ACK_MEMORY_LIMIT + 9)),
        Some(AckKind::Delivered)
    );
}
