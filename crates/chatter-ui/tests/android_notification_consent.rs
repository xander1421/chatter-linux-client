// Exercise the actual platform-independent production capability registry on host.
#[path = "../src/platform/android/call_notification_consent.rs"]
mod call_notification_consent;

use call_notification_consent::{ACCEPT, Consent, DECLINE, FULL_SCREEN, OPEN};
use std::time::{Duration, Instant};

const ACTIONS: [&str; 4] = [ACCEPT, OPEN, FULL_SCREEN, DECLINE];

#[test]
fn every_action_rejects_missing_forged_and_substituted_proofs_without_revoking_current() {
    let now = Instant::now();
    for (index, action) in ACTIONS.iter().enumerate() {
        let mut consent = Consent::new();
        let current = consent.issue("current-call", now, Duration::from_secs(60));
        let other = consent.issue("other-call", now, Duration::from_secs(60));
        assert!(!consent.consume("current-call", action, "", now));
        assert!(!consent.consume("current-call", action, "invalid-proof", now));
        assert!(!consent.consume("current-call", action, &other[index], now));
        for (wrong_index, proof) in current.iter().enumerate() {
            if wrong_index != index {
                assert!(!consent.consume("current-call", action, proof, now));
            }
        }
        assert!(consent.consume("current-call", action, &current[index], now));
        assert!(consent.consume("other-call", action, &other[index], now));
    }
}

#[test]
fn either_decision_rejects_its_replay_and_every_sibling_but_preserves_other_calls() {
    let now = Instant::now();
    for decision_index in [0, 3] {
        let mut consent = Consent::new();
        let current = consent.issue("current-call", now, Duration::from_secs(60));
        let other = consent.issue("other-call", now, Duration::from_secs(60));
        assert!(consent.consume("current-call", OPEN, &current[1], now));
        assert!(consent.consume("current-call", FULL_SCREEN, &current[2], now));
        assert!(consent.consume(
            "current-call",
            ACTIONS[decision_index],
            &current[decision_index],
            now
        ));
        for (index, action) in ACTIONS.iter().enumerate() {
            assert!(!consent.consume("current-call", action, &current[index], now));
        }
        assert!(consent.consume("other-call", DECLINE, &other[3], now));
    }
}

#[test]
fn every_action_fails_after_replacement_cancel_deadline_or_process_loss() {
    let now = Instant::now();
    let lifetime = Duration::from_secs(60);
    for (index, action) in ACTIONS.iter().enumerate() {
        let mut consent = Consent::new();
        let old = consent.issue("current-call", now, lifetime);
        let current = consent.issue("current-call", now, lifetime);
        assert!(!consent.consume("current-call", action, &old[index], now));
        // A stale decision must not retire its replacement generation.
        assert!(consent.consume("current-call", action, &current[index], now));

        let cancelled = consent.issue("current-call", now, lifetime);
        consent.revoke("current-call");
        assert!(!consent.consume("current-call", action, &cancelled[index], now));

        let expired = consent.issue("current-call", now, lifetime);
        assert!(!consent.consume("current-call", action, &expired[index], now + lifetime));
        let mut restarted = Consent::new();
        assert!(!restarted.consume("current-call", action, &expired[index], now));

        let live = consent.issue("current-call", now, lifetime);
        assert!(consent.consume(
            "current-call",
            action,
            &live[index],
            now + lifetime - Duration::from_nanos(1)
        ));
    }
}
