use super::*;
use crate::outbox::{DropReason, Sealer};
use crate::storage::MemoryStorage;
use crate::{MAX_COURIERS_PER_MESSAGE, MAX_SEND_ATTEMPTS, MESSAGE_TTL_SECS};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

// ---------------------------------------------------------------- fixtures

fn peer(n: u8) -> PeerId {
    PeerId::from_str(&format!("{n:016x}"))
}

fn key(n: u8) -> [u8; 32] {
    [n; 32]
}

fn snapshot(p: &PeerId, k: Option<[u8; 32]>, verified: bool) -> PeerSnapshot {
    PeerSnapshot {
        peer: p.clone(),
        is_connected: true,
        noise_public_key: k,
        is_verified: verified,
    }
}

/// The router never sees the sealing key, so tests do not need a real one.
struct PlainSealer;

impl Sealer for PlainSealer {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
        Ok(plaintext.to_vec())
    }
    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
        Ok(sealed.to_vec())
    }
}

#[derive(Default)]
struct MockState {
    connected: Vec<PeerId>,
    reachable: Vec<PeerId>,
    promptly: Vec<PeerId>,
    securely: Vec<PeerId>,
    snapshots: Vec<PeerSnapshot>,
    courier_accepts: bool,
    private_sends: Vec<(String, PeerId, String)>,
    courier_sends: Vec<(String, Vec<PeerId>)>,
}

/// Shared-state mock, so a test can hand the router a transport and keep
/// inspecting the same one afterwards.
#[derive(Clone, Default)]
struct MockTransport(Rc<RefCell<MockState>>);

impl MockTransport {
    fn new() -> Self {
        Self::default()
    }

    /// Branch 1: a live link with an established secure session.
    fn connected_secure(self, p: &PeerId) -> Self {
        {
            let mut s = self.0.borrow_mut();
            s.connected.push(p.clone());
            s.reachable.push(p.clone());
            s.promptly.push(p.clone());
            s.securely.push(p.clone());
        }
        self
    }

    /// Branch 2: connected, no secure session.
    fn connected_insecure(self, p: &PeerId) -> Self {
        {
            let mut s = self.0.borrow_mut();
            s.connected.push(p.clone());
            s.reachable.push(p.clone());
            s.promptly.push(p.clone());
        }
        self
    }

    /// Branch 3, prompt variant: the send really leaves the device.
    fn reachable_prompt(self, p: &PeerId) -> Self {
        {
            let mut s = self.0.borrow_mut();
            s.reachable.push(p.clone());
            s.promptly.push(p.clone());
        }
        self
    }

    /// Branch 3, queue variant: the send only joins a queue waiting on internet.
    fn reachable_slow(self, p: &PeerId) -> Self {
        self.0.borrow_mut().reachable.push(p.clone());
        self
    }

    fn with_snapshots(self, snapshots: Vec<PeerSnapshot>) -> Self {
        self.0.borrow_mut().snapshots = snapshots;
        self
    }

    fn accepting_couriers(self) -> Self {
        self.0.borrow_mut().courier_accepts = true;
        self
    }

    /// A courier walks into range after the send already happened.
    fn courier_arrives(&self, p: &PeerId, k: [u8; 32], verified: bool) {
        let mut s = self.0.borrow_mut();
        s.connected.push(p.clone());
        s.snapshots.push(snapshot(p, Some(k), verified));
    }

    fn becomes_connected(&self, p: &PeerId, securely: bool) {
        let mut s = self.0.borrow_mut();
        s.connected.push(p.clone());
        if securely {
            s.securely.push(p.clone());
        }
    }

    fn becomes_reachable(&self, p: &PeerId, promptly: bool) {
        let mut s = self.0.borrow_mut();
        s.reachable.push(p.clone());
        if promptly {
            s.promptly.push(p.clone());
        }
    }

    fn private_sends(&self) -> Vec<(String, PeerId, String)> {
        self.0.borrow().private_sends.clone()
    }

    fn courier_sends(&self) -> Vec<(String, Vec<PeerId>)> {
        self.0.borrow().courier_sends.clone()
    }

    fn boxed(&self) -> Box<dyn RouterTransport> {
        Box::new(self.clone())
    }
}

impl RouterTransport for MockTransport {
    fn is_peer_connected(&self, p: &PeerId) -> bool {
        self.0.borrow().connected.contains(p)
    }
    fn is_peer_reachable(&self, p: &PeerId) -> bool {
        self.0.borrow().reachable.contains(p)
    }
    fn can_deliver_promptly(&self, p: &PeerId) -> bool {
        self.0.borrow().promptly.contains(p)
    }
    fn can_deliver_securely(&self, p: &PeerId) -> bool {
        self.0.borrow().securely.contains(p)
    }
    fn peer_snapshots(&self) -> Vec<PeerSnapshot> {
        self.0.borrow().snapshots.clone()
    }
    fn send_private(&self, content: &str, to: &PeerId, message_id: &str) -> bool {
        self.0.borrow_mut().private_sends.push((
            content.to_string(),
            to.clone(),
            message_id.to_string(),
        ));
        true
    }
    fn send_courier(
        &self,
        _content: &str,
        message_id: &str,
        _recipient_key: &[u8; 32],
        via: &[PeerId],
    ) -> Vec<PeerId> {
        let accepts = self.0.borrow().courier_accepts;
        if !accepts {
            return Vec::new();
        }
        self.0
            .borrow_mut()
            .courier_sends
            .push((message_id.to_string(), via.to_vec()));
        via.to_vec()
    }
}

#[derive(Default)]
struct TestDirectory {
    keys: BTreeMap<PeerId, [u8; 32]>,
}

impl TestDirectory {
    fn with_key(mut self, p: &PeerId, k: [u8; 32]) -> Self {
        self.keys.insert(p.clone(), k);
        self
    }
}

impl CourierDirectory for TestDirectory {
    fn noise_key(&self, p: &PeerId) -> Option<[u8; 32]> {
        self.keys.get(p).copied()
    }
}

fn router(transports: Vec<Box<dyn RouterTransport>>, directory: TestDirectory) -> MessageRouter {
    let mut outbox = OutboxStore::new(Arc::new(MemoryStorage::new()), Arc::new(PlainSealer));
    // Load so the persistence path is genuinely exercised rather than deferred.
    let dropped = outbox.load().expect("cold load of an empty store");
    assert!(dropped.is_empty());
    MessageRouter::new(transports, Box::new(directory), outbox)
}

/// A bridge deposit that records every message ID it was asked to park.
fn recording_bridge(log: Rc<RefCell<Vec<String>>>, accepts: bool) -> BridgeDeposit {
    Box::new(move |_content, message_id, _recipient_key| {
        log.borrow_mut().push(message_id.to_string());
        accepts
    })
}

fn carried(events: &[RouterEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, RouterEvent::Carried { .. }))
        .count()
}

fn drops(events: &[RouterEvent]) -> Vec<(String, DropReason)> {
    events
        .iter()
        .filter_map(|e| match e {
            RouterEvent::Dropped(d) => Some((d.message_id.clone(), d.reason)),
            _ => None,
        })
        .collect()
}

fn no_storage_failures(events: &[RouterEvent]) {
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RouterEvent::StorageFailed { .. })),
        "unexpected storage failure: {events:?}"
    );
}

// ----------------------------------------------------------- the four branches

#[test]
fn branch_one_connected_and_secure_sends_without_retaining_or_couriering() {
    let recipient = peer(1);
    let courier = peer(2);
    let transport = MockTransport::new()
        .connected_secure(&recipient)
        .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
        .accepting_couriers();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);
    let bridge_log = Rc::new(RefCell::new(Vec::new()));
    router.set_bridge_deposit(recording_bridge(bridge_log.clone(), true));

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert_eq!(outcome.link, LinkState::Secure);
    assert!(outcome.events.is_empty());
    assert_eq!(transport.private_sends().len(), 1);
    // The strongest signal there is: nothing retained, nobody handed a copy,
    // nothing parked on a relay.
    assert!(router.outbox().queue(&recipient).is_empty());
    assert!(transport.courier_sends().is_empty());
    assert!(bridge_log.borrow().is_empty());
}

#[test]
fn branch_two_connected_without_a_session_sends_retains_and_couriers() {
    let recipient = peer(1);
    let courier = peer(2);
    let transport = MockTransport::new()
        .connected_insecure(&recipient)
        .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
        .accepting_couriers();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert_eq!(outcome.link, LinkState::Insecure);
    no_storage_failures(&outcome.events);
    assert_eq!(transport.private_sends().len(), 1);

    // Retained: a replayed announce could have bound this ID to a link whose
    // owner can never finish the handshake.
    let queue = router.outbox().queue(&recipient);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].send_attempts, 1);

    // And couriered. This is the deliberate metadata tradeoff — do not
    // "optimise" it away.
    assert_eq!(
        transport.courier_sends(),
        vec![("m1".to_string(), vec![courier])]
    );
    assert_eq!(carried(&outcome.events), 1);
}

#[test]
fn branch_three_reachable_and_prompt_retains_but_does_not_courier() {
    let recipient = peer(1);
    let courier = peer(2);
    let transport = MockTransport::new()
        .reachable_prompt(&recipient)
        .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
        .accepting_couriers();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert_eq!(outcome.link, LinkState::Reachable);
    assert_eq!(router.outbox().queue(&recipient).len(), 1);
    assert_eq!(router.outbox().queue(&recipient)[0].send_attempts, 1);
    // It really left the device; no need to spend courier slots on it.
    assert!(transport.courier_sends().is_empty());
    assert_eq!(carried(&outcome.events), 0);
}

#[test]
fn branch_three_reachable_but_only_queued_also_couriers() {
    let recipient = peer(1);
    let courier = peer(2);
    let transport = MockTransport::new()
        .reachable_slow(&recipient)
        .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
        .accepting_couriers();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    // Same LinkState as the prompt case — the *retention* rule is identical.
    assert_eq!(outcome.link, LinkState::Reachable);
    assert_eq!(router.outbox().queue(&recipient).len(), 1);
    // But the send only joined a queue waiting on internet that may never come,
    // so couriers get a copy too.
    assert_eq!(transport.courier_sends().len(), 1);
    assert_eq!(carried(&outcome.events), 1);
}

#[test]
fn branch_four_no_transport_retains_with_zero_attempts_and_couriers() {
    let recipient = peer(1);
    let courier = peer(2);
    let transport = MockTransport::new()
        .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
        .accepting_couriers();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert_eq!(outcome.link, LinkState::Unreachable);
    assert!(transport.private_sends().is_empty());
    let queue = router.outbox().queue(&recipient);
    assert_eq!(queue.len(), 1);
    // Nothing was transmitted, so nothing counts against the attempt cap.
    assert_eq!(queue[0].send_attempts, 0);
    assert_eq!(transport.courier_sends().len(), 1);
}

#[test]
fn a_recipient_with_no_known_static_key_cannot_be_couriered_to() {
    let recipient = peer(1);
    let courier = peer(2);
    let transport = MockTransport::new()
        .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
        .accepting_couriers();
    // No key for the recipient: nothing can be sealed to them.
    let directory = TestDirectory::default();
    let mut router = router(vec![transport.boxed()], directory);
    let log = Rc::new(RefCell::new(Vec::new()));
    router.set_bridge_deposit(recording_bridge(log.clone(), true));

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert_eq!(outcome.link, LinkState::Unreachable);
    assert!(transport.courier_sends().is_empty());
    assert!(log.borrow().is_empty());
    assert_eq!(carried(&outcome.events), 0);
    // Still retained: a direct delivery can still happen later.
    assert_eq!(router.outbox().queue(&recipient).len(), 1);
}

#[test]
fn a_refused_courier_deposit_is_not_recorded_as_carried() {
    let recipient = peer(1);
    let courier = peer(2);
    // Snapshots offer a courier, but the transport cannot courier at all.
    let transport =
        MockTransport::new().with_snapshots(vec![snapshot(&courier, Some(key(2)), true)]);
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert_eq!(carried(&outcome.events), 0);
    assert!(
        router.outbox().queue(&recipient)[0]
            .deposited_courier_keys
            .is_empty(),
        "a refused deposit must not burn a courier slot"
    );
}

// ------------------------------------------------------------ courier choice

#[test]
fn courier_selection_takes_verified_peers_in_snapshot_order_capped() {
    let recipient = peer(1);
    let (v1, v2, unverified, v3, v4) = (peer(10), peer(11), peer(12), peer(13), peer(14));
    let transport = MockTransport::new()
        .with_snapshots(vec![
            snapshot(&v1, Some(key(10)), true),
            snapshot(&v2, Some(key(11)), true),
            snapshot(&unverified, Some(key(12)), false),
            snapshot(&v3, Some(key(13)), true),
            snapshot(&v4, Some(key(14)), true),
        ])
        .accepting_couriers();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);

    let sends = transport.courier_sends();
    assert_eq!(sends.len(), 1);
    // Verified peers in snapshot order, the unverified one skipped, capped.
    assert_eq!(sends[0].1, vec![v1, v2, v3]);
    assert_eq!(sends[0].1.len(), MAX_COURIERS_PER_MESSAGE);
}

#[test]
fn courier_selection_excludes_the_recipient_unverified_and_keyless_peers() {
    let recipient = peer(1);
    let stranger = peer(10);
    let keyless = peer(12);
    let verified = peer(11);
    let transport = MockTransport::new()
        .with_snapshots(vec![
            // The recipient itself, matched by key: never hand someone their own
            // mail back as an opaque envelope.
            snapshot(&recipient, Some(key(1)), true),
            // Not verified: may not carry mail.
            snapshot(&stranger, Some(key(10)), false),
            // No static key at all: invisible to the courier path.
            snapshot(&keyless, None, true),
            snapshot(&verified, Some(key(11)), true),
        ])
        .accepting_couriers();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);

    let sends = transport.courier_sends();
    assert_eq!(sends[0].1, vec![verified]);
}

#[test]
fn a_disconnected_peer_is_not_a_courier() {
    let recipient = peer(1);
    let transport = MockTransport::new()
        .with_snapshots(vec![PeerSnapshot {
            peer: peer(10),
            is_connected: false,
            noise_public_key: Some(key(10)),
            is_verified: true,
        }])
        .accepting_couriers();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert!(transport.courier_sends().is_empty());
}

#[test]
fn a_courier_already_carrying_a_message_is_not_asked_twice() {
    let recipient = peer(1);
    let first = peer(10);
    let second = peer(11);
    let transport = MockTransport::new()
        .with_snapshots(vec![snapshot(&first, Some(key(10)), true)])
        .accepting_couriers();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    assert_eq!(
        transport.courier_sends(),
        vec![("m1".to_string(), vec![first])]
    );

    // A second courier appears and the same message ID is re-sent. The outbox
    // preserves the deposit record across the replacement, so the first courier
    // is excluded rather than re-burned.
    transport
        .0
        .borrow_mut()
        .snapshots
        .push(snapshot(&second, Some(key(11)), true));
    router.send_private("hi", &recipient, "bob", "m1", 1_100);

    let sends = transport.courier_sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[1].1, vec![second]);
}

// ------------------------------------------------- courier_became_available

#[test]
fn a_newly_connected_courier_picks_up_mail_it_is_not_already_carrying() {
    let recipient = peer(1);
    let courier = peer(2);
    // Nobody around at send time.
    let transport = MockTransport::new().accepting_couriers();
    let directory = TestDirectory::default()
        .with_key(&recipient, key(1))
        .with_key(&courier, key(2));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    assert!(transport.courier_sends().is_empty());

    // Now a verified courier walks in. This is what turns couriering from "a
    // courier happened to be nearby at send time" into eventual spread.
    transport.courier_arrives(&courier, key(2), true);
    let events = router.courier_became_available(&courier, 1_100);

    assert_eq!(
        transport.courier_sends(),
        vec![("m1".to_string(), vec![courier.clone()])]
    );
    assert_eq!(carried(&events), 1);
    assert!(router.outbox().queue(&recipient)[0].is_carried_by(&key(2)));

    // A second announce from the same peer must not re-burn a slot on a copy
    // they already hold.
    let again = router.courier_became_available(&courier, 1_200);
    assert_eq!(transport.courier_sends().len(), 1);
    assert_eq!(carried(&again), 0);
}

#[test]
fn a_courier_is_never_handed_mail_addressed_to_itself() {
    let courier = peer(2);
    let transport = MockTransport::new().accepting_couriers();
    let directory = TestDirectory::default().with_key(&courier, key(2));
    let mut router = router(vec![transport.boxed()], directory);

    // Mail *for* the courier. It flushes directly on connect instead.
    router.send_private("hi", &courier, "bob", "m1", 1_000);
    transport.courier_arrives(&courier, key(2), true);

    let events = router.courier_became_available(&courier, 1_100);

    assert!(transport.courier_sends().is_empty());
    assert_eq!(carried(&events), 0);
}

#[test]
fn an_unverified_peer_may_not_pick_up_mail() {
    let recipient = peer(1);
    let stranger = peer(2);
    let transport = MockTransport::new().accepting_couriers();
    let directory = TestDirectory::default()
        .with_key(&recipient, key(1))
        .with_key(&stranger, key(2));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    transport.courier_arrives(&stranger, key(2), false);

    let events = router.courier_became_available(&stranger, 1_100);

    assert!(transport.courier_sends().is_empty());
    assert_eq!(carried(&events), 0);
}

#[test]
fn a_verified_stranger_may_pick_up_mail() {
    let recipient = peer(1);
    let stranger = peer(2);
    let transport = MockTransport::new().accepting_couriers();
    let directory = TestDirectory::default()
        .with_key(&recipient, key(1))
        .with_key(&stranger, key(2));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    transport.courier_arrives(&stranger, key(2), true);

    let events = router.courier_became_available(&stranger, 1_100);

    assert_eq!(transport.courier_sends().len(), 1);
    assert_eq!(carried(&events), 1);
}

#[test]
fn expired_mail_is_not_handed_to_a_new_courier() {
    let recipient = peer(1);
    let courier = peer(2);
    let transport = MockTransport::new().accepting_couriers();
    let directory = TestDirectory::default()
        .with_key(&recipient, key(1))
        .with_key(&courier, key(2));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    transport.courier_arrives(&courier, key(2), true);

    let events = router.courier_became_available(&courier, 1_000 + MESSAGE_TTL_SECS + 1);

    assert!(transport.courier_sends().is_empty());
    assert_eq!(carried(&events), 0);
}

// ------------------------------------------------------------ read receipts

#[test]
fn a_read_receipt_with_no_reachable_transport_is_not_recorded_as_sent() {
    let sender = peer(1);
    let transport = MockTransport::new();
    let router = router(vec![transport.boxed()], TestDirectory::default());

    let accepted = router.send_read_receipt(&sender, |t| t.send_private("r", &peer(1), "r1"));

    // False means "dropped, no route". Recording it as sent would leave the
    // sender's message unread forever; it is retried on the next read scan.
    assert!(!accepted);
    assert!(transport.private_sends().is_empty());
}

#[test]
fn a_read_receipt_goes_over_the_first_reachable_transport() {
    let sender = peer(1);
    let dead = MockTransport::new();
    let live = MockTransport::new().reachable_prompt(&sender);
    let router = router(vec![dead.boxed(), live.boxed()], TestDirectory::default());

    let accepted = router.send_read_receipt(&sender, |t| t.send_private("r", &peer(1), "r1"));

    assert!(accepted);
    assert!(dead.private_sends().is_empty());
    assert_eq!(live.private_sends().len(), 1);
}

#[test]
fn a_read_receipt_the_transport_refuses_is_not_recorded_as_sent() {
    let sender = peer(1);
    let transport = MockTransport::new().reachable_prompt(&sender);
    let router = router(vec![transport.boxed()], TestDirectory::default());

    // Reachable, but the wire call itself failed. Same consequence as no route:
    // it must not be recorded as sent.
    assert!(!router.send_read_receipt(&sender, |_| false));
}

// ----------------------------------------------------------------- flushes

#[test]
fn a_secure_flush_sends_and_stops_retaining() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    assert_eq!(router.outbox().queue(&recipient).len(), 1);

    transport.becomes_connected(&recipient, true);
    let events = router.flush_outbox(&recipient, 1_100);

    assert_eq!(transport.private_sends().len(), 1);
    assert!(router.outbox().queue(&recipient).is_empty());
    assert!(drops(&events).is_empty());
    no_storage_failures(&events);
}

#[test]
fn an_insecure_flush_never_counts_toward_the_attempt_cap() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    transport.becomes_connected(&recipient, false);

    // A handshake stalling across reconnect flapping must not burn the cap and
    // lose the very copy the retention exists to preserve.
    for _ in 0..(MAX_SEND_ATTEMPTS * 3) {
        let events = router.flush_outbox(&recipient, 1_100);
        assert!(drops(&events).is_empty(), "insecure flush must not drop");
    }

    let queue = router.outbox().queue(&recipient);
    assert_eq!(queue.len(), 1, "the retained copy must survive");
    assert_eq!(queue[0].send_attempts, 0);
    assert_eq!(
        transport.private_sends().len() as u32,
        MAX_SEND_ATTEMPTS * 3,
        "every flush still transmits over the live link"
    );
}

#[test]
fn a_reachable_flush_counts_attempts_and_drops_at_the_cap() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    // Branch 4 retains at zero attempts.
    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    transport.becomes_reachable(&recipient, true);

    for expected in 1..=MAX_SEND_ATTEMPTS {
        let events = router.flush_outbox(&recipient, 1_100);
        assert!(drops(&events).is_empty());
        assert_eq!(router.outbox().queue(&recipient)[0].send_attempts, expected);
    }

    // The cap is reached: give up visibly rather than leaving the message on
    // "sending" forever.
    let events = router.flush_outbox(&recipient, 1_100);
    assert_eq!(
        drops(&events),
        vec![("m1".to_string(), DropReason::AttemptsExhausted)]
    );
    assert!(router.outbox().queue(&recipient).is_empty());
    assert_eq!(transport.private_sends().len() as u32, MAX_SEND_ATTEMPTS);
}

#[test]
fn a_flush_with_no_route_leaves_the_message_untouched() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    for _ in 0..(MAX_SEND_ATTEMPTS * 2) {
        router.flush_outbox(&recipient, 1_100);
    }

    let queue = router.outbox().queue(&recipient);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].send_attempts, 0);
    assert!(transport.private_sends().is_empty());
}

#[test]
fn a_flush_drops_expired_mail_instead_of_sending_it() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    transport.becomes_connected(&recipient, true);

    let events = router.flush_outbox(&recipient, 1_000 + MESSAGE_TTL_SECS + 1);

    assert_eq!(
        drops(&events),
        vec![("m1".to_string(), DropReason::Expired)]
    );
    assert!(transport.private_sends().is_empty());
    assert!(router.outbox().queue(&recipient).is_empty());
}

#[test]
fn flush_all_covers_every_peer_with_retained_mail() {
    let (a, b) = (peer(1), peer(2));
    let transport = MockTransport::new();
    let directory = TestDirectory::default()
        .with_key(&a, key(1))
        .with_key(&b, key(2));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("x", &a, "ann", "m1", 1_000);
    router.send_private("y", &b, "ben", "m2", 1_000);
    transport.becomes_connected(&a, true);
    transport.becomes_connected(&b, true);

    router.flush_all_outbox(1_100);

    assert_eq!(transport.private_sends().len(), 2);
    assert!(router.outbox().is_empty());
}

#[test]
fn cleanup_expires_stale_mail_and_reports_it() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("old", &recipient, "bob", "m1", 1_000);
    router.send_private("new", &recipient, "bob", "m2", 1_000 + MESSAGE_TTL_SECS);

    let events = router.cleanup_expired(1_000 + MESSAGE_TTL_SECS + 1);

    assert_eq!(
        drops(&events),
        vec![("m1".to_string(), DropReason::Expired)]
    );
    assert_eq!(router.outbox().queue(&recipient).len(), 1);
}

#[test]
fn an_ack_stops_the_retention_and_a_read_ack_is_never_downgraded() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);

    let (outcome, events) = router.mark_acked(&recipient, "m1", AckKind::Read);
    assert!(outcome.cleared);
    assert_eq!(outcome.strongest, AckKind::Read);
    no_storage_failures(&events);
    assert!(router.outbox().queue(&recipient).is_empty());

    // A delivery ack landing afterwards must not downgrade the read.
    let (late, _) = router.mark_acked(&recipient, "m1", AckKind::Delivered);
    assert!(!late.cleared);
    assert_eq!(late.strongest, AckKind::Read);
}

// ------------------------------------------------------------ bridge drops

#[test]
fn the_bridge_drop_runs_even_with_no_connected_courier() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);
    let log = Rc::new(RefCell::new(Vec::new()));
    router.set_bridge_deposit(recording_bridge(log.clone(), true));

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert_eq!(*log.borrow(), vec!["m1".to_string()]);
    assert_eq!(carried(&outcome.events), 1);
}

#[test]
fn a_rejected_bridge_drop_is_not_reported_as_carried() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);
    let log = Rc::new(RefCell::new(Vec::new()));
    router.set_bridge_deposit(recording_bridge(log.clone(), false));

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert_eq!(log.borrow().len(), 1);
    // A socket write followed by a relay rejection must not show as "carried".
    assert_eq!(carried(&outcome.events), 0);
}

#[test]
fn the_bridge_retry_sweep_skips_peers_a_transport_can_promptly_reach() {
    let stalled = peer(1);
    let live = peer(2);
    let transport = MockTransport::new();
    let directory = TestDirectory::default()
        .with_key(&stalled, key(1))
        .with_key(&live, key(2));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("a", &stalled, "ann", "m1", 1_000);
    router.send_private("b", &live, "ben", "m2", 1_000);

    // Wired only now, so the original sends never deposited: exactly the race
    // the sweep exists to cover.
    let log = Rc::new(RefCell::new(Vec::new()));
    router.set_bridge_deposit(recording_bridge(log.clone(), true));
    transport.becomes_reachable(&live, true);

    let events = router.retry_bridge_deposits(1_100);

    assert_eq!(*log.borrow(), vec!["m1".to_string()]);
    assert_eq!(carried(&events), 1);
}

#[test]
fn the_bridge_retry_sweep_still_fires_for_a_peer_reachable_but_not_promptly() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    let log = Rc::new(RefCell::new(Vec::new()));
    router.set_bridge_deposit(recording_bridge(log.clone(), true));
    // Reachable, but the send would only join a queue.
    transport.becomes_reachable(&recipient, false);

    router.retry_bridge_deposits(1_100);

    assert_eq!(*log.borrow(), vec!["m1".to_string()]);
}

#[test]
fn the_bridge_retry_sweep_ignores_expired_mail() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    let log = Rc::new(RefCell::new(Vec::new()));
    router.set_bridge_deposit(recording_bridge(log.clone(), true));

    router.retry_bridge_deposits(1_000 + MESSAGE_TTL_SECS + 1);

    assert!(log.borrow().is_empty());
}

#[test]
fn recovery_flushes_and_retries_the_bridge() {
    let recipient = peer(1);
    let transport = MockTransport::new();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![transport.boxed()], directory);
    let log = Rc::new(RefCell::new(Vec::new()));
    router.set_bridge_deposit(recording_bridge(log.clone(), true));

    router.send_private("hi", &recipient, "bob", "m1", 1_000);
    log.borrow_mut().clear();
    transport.becomes_reachable(&recipient, true);

    let events = router.recover_outbox(1_100);

    no_storage_failures(&events);
    // The flush re-sent it over the reachable transport...
    assert_eq!(transport.private_sends().len(), 1);
    // ...and the sweep skipped it, because that transport can deliver promptly.
    assert!(log.borrow().is_empty());
}

// ----------------------------------------------------- transport precedence

#[test]
fn the_first_connected_transport_decides_even_without_a_secure_session() {
    // bitchat picks the first connected transport and stays with it. Falling
    // through to a *different* transport's secure session would be a different
    // decision than the tree describes.
    let recipient = peer(1);
    let insecure = MockTransport::new().connected_insecure(&recipient);
    let secure = MockTransport::new().connected_secure(&recipient);
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![insecure.boxed(), secure.boxed()], directory);

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert_eq!(outcome.link, LinkState::Insecure);
    assert_eq!(insecure.private_sends().len(), 1);
    assert!(secure.private_sends().is_empty());
    assert_eq!(router.outbox().queue(&recipient).len(), 1);
}

#[test]
fn a_connected_transport_outranks_a_reachable_one() {
    let recipient = peer(1);
    let reachable = MockTransport::new().reachable_prompt(&recipient);
    let connected = MockTransport::new().connected_secure(&recipient);
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![reachable.boxed(), connected.boxed()], directory);

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert_eq!(outcome.link, LinkState::Secure);
    assert!(reachable.private_sends().is_empty());
    assert_eq!(connected.private_sends().len(), 1);
}

#[test]
fn couriers_are_tried_on_every_transport_until_one_accepts() {
    let recipient = peer(1);
    let courier = peer(2);
    // The first transport knows the courier but refuses to carry.
    let refuses = MockTransport::new().with_snapshots(vec![snapshot(&courier, Some(key(2)), true)]);
    let accepts = MockTransport::new()
        .with_snapshots(vec![snapshot(&courier, Some(key(2)), true)])
        .accepting_couriers();
    let directory = TestDirectory::default().with_key(&recipient, key(1));
    let mut router = router(vec![refuses.boxed(), accepts.boxed()], directory);

    let outcome = router.send_private("hi", &recipient, "bob", "m1", 1_000);

    assert!(refuses.courier_sends().is_empty());
    assert_eq!(accepts.courier_sends().len(), 1);
    assert_eq!(carried(&outcome.events), 1);
}
