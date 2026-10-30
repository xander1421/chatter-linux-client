// MARK: outright connect failures (a BlueZ-only divergence from iOS)

use super::{CONNECT_FAILURE_FORGET_MS, CONNECT_FAILURE_RETRY_DELAY_MS, MAX_CONNECT_FAILURES};

#[test]
fn one_failure_holds_the_address_for_the_retry_delay() {
    // The behaviour this exists for: 2s of spacing with no per-failure window
    // produced seven consecutive failures to one address in 19s on BlueZ.
    let mut s = ConnectionScheduler::default();
    s.note_connect_failed("aa", 1_000);

    assert!(s.is_failing_too_fast("aa", 1_000));
    assert!(s.is_failing_too_fast("aa", 1_000 + CONNECT_FAILURE_RETRY_DELAY_MS - 1));
    assert!(!s.is_failing_too_fast("aa", 1_000 + CONNECT_FAILURE_RETRY_DELAY_MS));
}

#[test]
fn the_cap_bites_after_three_failures_in_a_row() {
    let mut s = ConnectionScheduler::default();
    let mut now = 0;
    // Spaced so each one is allowed when it happens, per the delay above.
    for _ in 0..MAX_CONNECT_FAILURES {
        s.note_connect_failed("aa", now);
        now += CONNECT_FAILURE_RETRY_DELAY_MS;
    }
    // Past the delay, but the cap is reached, so still refused.
    assert!(s.is_failing_too_fast("aa", now));
}

#[test]
fn a_stale_record_starts_the_count_again() {
    // One bad patch of radio must not blacklist a peer for the session.
    let mut s = ConnectionScheduler::default();
    let mut now = 0;
    for _ in 0..MAX_CONNECT_FAILURES {
        s.note_connect_failed("aa", now);
        now += CONNECT_FAILURE_RETRY_DELAY_MS;
    }
    assert!(s.is_failing_too_fast("aa", now));

    let much_later = now + CONNECT_FAILURE_FORGET_MS + 1;
    assert!(
        !s.is_failing_too_fast("aa", much_later),
        "the record aged out"
    );

    // And the next failure counts as the first, not the fourth.
    s.note_connect_failed("aa", much_later);
    assert!(
        !s.is_failing_too_fast("aa", much_later + CONNECT_FAILURE_RETRY_DELAY_MS),
        "a reset count must not still be capped"
    );
}

#[test]
fn a_successful_connect_forgets_the_failures() {
    let mut s = ConnectionScheduler::default();
    let mut now = 0;
    for _ in 0..MAX_CONNECT_FAILURES {
        s.note_connect_failed("aa", now);
        now += CONNECT_FAILURE_RETRY_DELAY_MS;
    }
    assert!(s.is_failing_too_fast("aa", now));

    s.note_connected("aa");
    assert!(
        !s.is_failing_too_fast("aa", now),
        "success clears the record"
    );
}

#[test]
fn failures_are_tracked_per_address() {
    let mut s = ConnectionScheduler::default();
    s.note_connect_failed("aa", 0);
    assert!(s.is_failing_too_fast("aa", 0));
    assert!(
        !s.is_failing_too_fast("bb", 0),
        "an unrelated peer is unaffected"
    );
}
use super::*;

const P1: &str = "AA:BB:CC:DD:EE:01";
const P2: &str = "AA:BB:CC:DD:EE:02";

fn candidate(address: &str, rssi: i16, now_ms: u64) -> ConnectionCandidate {
    ConnectionCandidate::new(address, rssi, now_ms)
}

fn discover(
    scheduler: &mut ConnectionScheduler,
    address: &str,
    rssi: i16,
    now_ms: u64,
) -> DiscoveryDecision {
    scheduler.handle_discovery(
        &candidate(address, rssi, now_ms),
        0,
        None,
        PeripheralConnectionState::Disconnected,
        now_ms,
    )
}

fn never_busy(_: &str) -> bool {
    false
}

// -- RSSI gate on discovery ------------------------------------------------

#[test]
fn advert_above_the_gate_connects_and_one_at_it_does_not() {
    let mut s = ConnectionScheduler::new();
    assert_eq!(s.rssi_threshold(), -90);

    // Strictly greater than the threshold is the only thing that connects.
    assert_eq!(
        discover(&mut s, P1, -89, 1_000),
        DiscoveryDecision::ConnectNow
    );
    assert_eq!(s.candidate_count(), 0);

    assert_eq!(discover(&mut s, P2, -90, 1_000), DiscoveryDecision::Queued);
    assert_eq!(s.candidate_count(), 1);

    assert_eq!(discover(&mut s, P2, -91, 1_000), DiscoveryDecision::Queued);
    assert_eq!(
        s.candidate_count(),
        1,
        "same address replaces, never duplicates"
    );
}

#[test]
fn unconnectable_adverts_are_dropped_not_queued() {
    let mut s = ConnectionScheduler::new();
    let c = candidate(P1, -40, 1_000).with_connectable(false);
    let decision = s.handle_discovery(&c, 0, None, PeripheralConnectionState::Disconnected, 1_000);
    assert_eq!(decision, DiscoveryDecision::Ignore);
    assert_eq!(s.candidate_count(), 0);
}

// -- Connection budget -----------------------------------------------------

#[test]
fn budget_boundary_five_connects_six_and_seven_queue() {
    let mut s = ConnectionScheduler::new();
    assert_eq!(
        s.handle_discovery(
            &candidate(P1, -50, 1_000),
            5,
            None,
            PeripheralConnectionState::Disconnected,
            1_000
        ),
        DiscoveryDecision::ConnectNow
    );

    let mut s = ConnectionScheduler::new();
    assert_eq!(
        s.handle_discovery(
            &candidate(P1, -50, 1_000),
            6,
            None,
            PeripheralConnectionState::Disconnected,
            1_000
        ),
        DiscoveryDecision::Queued
    );

    let mut s = ConnectionScheduler::new();
    assert_eq!(
        s.handle_discovery(
            &candidate(P1, -50, 1_000),
            7,
            None,
            PeripheralConnectionState::Disconnected,
            1_000
        ),
        DiscoveryDecision::Queued
    );
}

#[test]
fn queue_drain_respects_the_budget_boundary() {
    let mut s = ConnectionScheduler::new();
    s.enqueue(candidate(P1, -50, 1_000));

    assert_eq!(s.next_candidate(6, never_busy, 2_000), QueueDecision::Idle);
    assert_eq!(s.next_candidate(7, never_busy, 2_000), QueueDecision::Idle);
    assert!(matches!(
        s.next_candidate(5, never_busy, 2_000),
        QueueDecision::Connect(_)
    ));
}

// -- Connect rate limit ----------------------------------------------------

#[test]
fn rate_limit_boundary_at_499_500_and_501_ms() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_attempt(10_000);

    // 499 ms: still limited, and the retry is scheduled just past expiry.
    assert_eq!(
        s.handle_discovery(
            &candidate(P1, -50, 10_499),
            0,
            None,
            PeripheralConnectionState::Disconnected,
            10_499
        ),
        DiscoveryDecision::ScheduleRetry { after_ms: 51 }
    );
    assert_eq!(
        s.candidate_count(),
        1,
        "rate-limited adverts are kept, not lost"
    );

    // Exactly 500 ms: the limit has expired.
    let mut s = ConnectionScheduler::new();
    s.note_connect_attempt(10_000);
    assert_eq!(
        s.handle_discovery(
            &candidate(P1, -50, 10_500),
            0,
            None,
            PeripheralConnectionState::Disconnected,
            10_500
        ),
        DiscoveryDecision::ConnectNow
    );

    // 501 ms.
    let mut s = ConnectionScheduler::new();
    s.note_connect_attempt(10_000);
    assert_eq!(
        s.handle_discovery(
            &candidate(P1, -50, 10_501),
            0,
            None,
            PeripheralConnectionState::Disconnected,
            10_501
        ),
        DiscoveryDecision::ConnectNow
    );
}

#[test]
fn rate_limit_delay_shrinks_as_the_window_expires() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_attempt(10_000);
    s.enqueue(candidate(P1, -50, 10_000));

    assert_eq!(
        s.next_candidate(0, never_busy, 10_000),
        QueueDecision::RetryAfter { after_ms: 550 }
    );
    assert_eq!(
        s.next_candidate(0, never_busy, 10_250),
        QueueDecision::RetryAfter { after_ms: 300 }
    );
    assert_eq!(
        s.next_candidate(0, never_busy, 10_499),
        QueueDecision::RetryAfter { after_ms: 51 }
    );
    assert!(matches!(
        s.next_candidate(0, never_busy, 10_500),
        QueueDecision::Connect(_)
    ));
}

#[test]
fn no_attempt_yet_means_no_rate_limit() {
    // Swift seeds this with `.distantPast`; `None` has to behave the same at
    // t=0, which is exactly when a fresh app meets its first advert.
    let mut s = ConnectionScheduler::new();
    assert_eq!(discover(&mut s, P1, -50, 0), DiscoveryDecision::ConnectNow);
}

// -- Existing link state ---------------------------------------------------

#[test]
fn adverts_for_links_we_already_hold_are_ignored() {
    let mut s = ConnectionScheduler::new();
    for state in [
        ExistingConnectionState {
            is_connected: true,
            ..Default::default()
        },
        ExistingConnectionState {
            is_connecting: true,
            ..Default::default()
        },
    ] {
        assert_eq!(
            s.handle_discovery(
                &candidate(P1, -50, 1_000),
                0,
                Some(state),
                PeripheralConnectionState::Disconnected,
                1_000
            ),
            DiscoveryDecision::Ignore
        );
    }
}

#[test]
fn recent_attempt_window_boundary_is_two_seconds() {
    let mut s = ConnectionScheduler::new();
    let state = ExistingConnectionState {
        last_connect_attempt_ms: Some(10_000),
        ..Default::default()
    };

    assert_eq!(
        s.handle_discovery(
            &candidate(P1, -50, 11_999),
            0,
            Some(state),
            PeripheralConnectionState::Disconnected,
            11_999
        ),
        DiscoveryDecision::Ignore
    );
    assert_eq!(
        s.handle_discovery(
            &candidate(P1, -50, 12_000),
            0,
            Some(state),
            PeripheralConnectionState::Disconnected,
            12_000
        ),
        DiscoveryDecision::ConnectNow
    );
}

#[test]
fn radio_state_out_of_step_with_ours_cancels_the_stale_connection() {
    let mut s = ConnectionScheduler::new();
    for state in [
        PeripheralConnectionState::Connecting,
        PeripheralConnectionState::Connected,
    ] {
        assert_eq!(
            s.handle_discovery(&candidate(P1, -50, 1_000), 0, None, state, 1_000),
            DiscoveryDecision::CancelStaleConnection
        );
    }
}

// -- Per-peer backoff ------------------------------------------------------

#[test]
fn connect_timeout_ignore_window_boundary_is_fifteen_seconds() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_timeout(P1, 10_000);

    assert_eq!(discover(&mut s, P1, -50, 24_999), DiscoveryDecision::Ignore);
    assert_eq!(
        discover(&mut s, P1, -50, 25_000),
        DiscoveryDecision::ConnectNow
    );
}

#[test]
fn a_rotated_address_of_a_peer_we_hold_is_ignored_for_five_minutes() {
    let mut s = ConnectionScheduler::new();
    s.note_duplicate_peer(P1, 10_000);

    assert_eq!(
        discover(&mut s, P1, -50, 309_999),
        DiscoveryDecision::Ignore
    );
    assert_eq!(
        discover(&mut s, P1, -50, 310_000),
        DiscoveryDecision::ConnectNow
    );
}

#[test]
fn a_duplicate_address_is_not_charged_as_a_failure() {
    // The distinction the separate map exists for. Nothing failed — the peer is
    // reachable, on another link — so the address must not accumulate the
    // backoff that governs genuinely broken peers, or a phone that rotates a
    // few times would poison its own future addresses' shared failure budget.
    let mut s = ConnectionScheduler::new();
    s.note_duplicate_peer(P1, 10_000);
    s.prune_backoff(10_000);

    assert_eq!(
        discover(&mut s, P1, -50, 310_000),
        DiscoveryDecision::ConnectNow,
        "the window expired but something else still refuses the address"
    );
}

#[test]
fn disconnect_ignore_window_boundary_is_three_seconds() {
    let mut s = ConnectionScheduler::new();
    s.note_disconnected(P1, 10_000);

    assert_eq!(discover(&mut s, P1, -50, 12_999), DiscoveryDecision::Ignore);
    assert_eq!(
        discover(&mut s, P1, -50, 13_000),
        DiscoveryDecision::ConnectNow
    );
}

#[test]
fn a_successful_connect_clears_every_form_of_backoff() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_timeout(P1, 10_000);
    s.note_disconnected(P1, 10_000);
    assert_eq!(s.failure_count(P1), 1);
    assert_eq!(s.backoff_len(), 2);

    s.note_connected(P1);

    assert_eq!(s.failure_count(P1), 0);
    assert_eq!(s.backoff_len(), 0);
    assert_eq!(
        discover(&mut s, P1, -50, 10_100),
        DiscoveryDecision::ConnectNow
    );
}

#[test]
fn weak_link_cooldown_defers_a_timed_out_peer_that_is_still_weak() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_timeout(P1, 10_000);
    s.enqueue(candidate(P1, -95, 10_000));

    // 25 s of cooldown remain; the retry is clamped to the 15 s ceiling.
    assert_eq!(
        s.next_candidate(0, never_busy, 15_000),
        QueueDecision::RetryAfter { after_ms: 15_000 }
    );
    assert_eq!(s.candidate_count(), 1, "deferred candidates are put back");
}

#[test]
fn weak_link_retry_delay_is_clamped_at_both_ends() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_timeout(P1, 10_000);

    // 29.5 s in: 500 ms nominally remain, raised to the 2 s floor.
    s.enqueue(candidate(P1, -95, 10_000));
    assert_eq!(
        s.next_candidate(0, never_busy, 39_500),
        QueueDecision::RetryAfter { after_ms: 2_000 }
    );

    // Exactly 30 s: the cooldown has expired, so it connects.
    assert!(matches!(
        s.next_candidate(0, never_busy, 40_000),
        QueueDecision::Connect(_)
    ));
}

#[test]
fn a_timed_out_peer_that_got_louder_skips_the_cooldown() {
    // The cutoff is -90; -89 is above it, so the peer is assumed to have moved
    // closer and is worth another attempt immediately.
    let mut s = ConnectionScheduler::new();
    s.note_connect_timeout(P1, 10_000);

    s.enqueue(candidate(P1, -90, 10_000));
    assert_eq!(
        s.next_candidate(0, never_busy, 15_000),
        QueueDecision::RetryAfter { after_ms: 15_000 }
    );

    s.enqueue(candidate(P1, -89, 10_000));
    assert!(matches!(
        s.next_candidate(0, never_busy, 15_000),
        QueueDecision::Connect(_)
    ));
}

#[test]
fn disconnect_settle_window_holds_on_the_queue_drain_path() {
    // A weak advert is queued even while the link is up, so the post-disconnect
    // drain would reconnect instantly if the window were only checked on
    // discovery.
    let mut s = ConnectionScheduler::new();
    s.enqueue(candidate(P1, -95, 10_000));
    s.note_disconnected(P1, 10_000);

    assert_eq!(
        s.next_candidate(0, never_busy, 10_100),
        QueueDecision::RetryAfter { after_ms: 2_950 }
    );
    assert_eq!(s.candidate_count(), 1);

    assert!(matches!(
        s.next_candidate(0, never_busy, 13_000),
        QueueDecision::Connect(_)
    ));
}

// -- Candidate queue -------------------------------------------------------

#[test]
fn enqueue_replaces_rather_than_duplicating() {
    let mut s = ConnectionScheduler::new();
    s.enqueue(candidate(P1, -90, 1_000));
    s.enqueue(candidate(P1, -55, 2_000));
    assert_eq!(s.candidate_count(), 1);

    match s.next_candidate(0, never_busy, 3_000) {
        QueueDecision::Connect(c) => {
            assert_eq!(c.address, P1);
            assert_eq!(c.rssi, -55);
        }
        other => panic!("expected a connect, got {other:?}"),
    }
    assert_eq!(s.candidate_count(), 0);
}

#[test]
fn the_candidate_cap_drops_the_weakest() {
    let config = SchedulerConfig {
        candidate_cap: 3,
        ..Default::default()
    };
    let mut s = ConnectionScheduler::with_config(config);
    for (i, rssi) in [-40i16, -50, -60, -70, -80].iter().enumerate() {
        s.enqueue(candidate(&format!("addr-{i}"), *rssi, 1_000));
    }

    assert_eq!(s.candidate_count(), 3);
    let kept: Vec<i16> = (0..3)
        .filter_map(|_| match s.next_candidate(0, never_busy, 1_000) {
            QueueDecision::Connect(c) => Some(c.rssi),
            _ => None,
        })
        .collect();
    assert_eq!(kept, vec![-40, -50, -60]);
}

#[test]
fn the_hundred_candidate_cap_is_the_ported_default() {
    let mut s = ConnectionScheduler::new();
    for i in 0..150 {
        s.enqueue(candidate(&format!("addr-{i:03}"), -60, 1_000));
    }
    assert_eq!(s.candidate_count(), CONNECTION_CANDIDATES_MAX);
}

#[test]
fn strongest_candidate_wins() {
    let mut s = ConnectionScheduler::new();
    s.enqueue(candidate("weak", -88, 1_000));
    s.enqueue(candidate("strong", -60, 2_000));

    match s.next_candidate(0, never_busy, 3_000) {
        QueueDecision::Connect(c) => assert_eq!(c.address, "strong"),
        other => panic!("expected a connect, got {other:?}"),
    }
}

#[test]
fn a_stale_advert_loses_to_a_fresh_one_of_equal_strength() {
    // 1 point per 100 ms of staleness: 10 s of age is 100 points, enough to
    // overcome 2 points per dBm of a 5 dBm advantage.
    let mut s = ConnectionScheduler::new();
    s.enqueue(candidate("stale", -60, 0));
    s.enqueue(candidate("fresh", -65, 10_000));

    match s.next_candidate(0, never_busy, 10_000) {
        QueueDecision::Connect(c) => assert_eq!(c.address, "fresh"),
        other => panic!("expected a connect, got {other:?}"),
    }
}

#[test]
fn history_nudges_the_order_without_overturning_signal() {
    // Worst-case history is a 16-point failure penalty plus a 10-point timeout
    // bias: 26 points, or 13 dBm of equivalent signal. A peer with a *worse*
    // record still wins when it is clearly closer, which is the point — history
    // breaks near-ties, it does not override the radio.
    let mut s = ConnectionScheduler::new();
    for _ in 0..10 {
        s.note_connect_failed("flaky", 0);
    }
    s.note_connect_timeout("flaky", 0);

    // 10 dBm ahead (20 points) is not enough to carry 26 points of history.
    s.enqueue(candidate("flaky", -60, 1_000));
    s.enqueue(candidate("clean", -70, 1_000));
    match s.next_candidate(0, never_busy, 1_000) {
        QueueDecision::Connect(c) => assert_eq!(c.address, "clean"),
        other => panic!("expected a connect, got {other:?}"),
    }

    // 14 dBm ahead (28 points) is.
    s.enqueue(candidate("flaky", -60, 1_000));
    s.enqueue(candidate("clean", -74, 1_000));
    match s.next_candidate(0, never_busy, 1_000) {
        QueueDecision::Connect(c) => assert_eq!(c.address, "flaky"),
        other => panic!("expected a connect, got {other:?}"),
    }
}

#[test]
fn failure_penalty_doubles_then_saturates_at_sixteen() {
    // `1 << min(4, failures)`: even a spotless peer carries 1 point, so the
    // deltas from clean are 1, 3, 7, 15 and then flat.
    let mut s = ConnectionScheduler::new();
    let c = candidate(P1, -60, 0);
    let spotless = s.score(&c, 0);
    assert_eq!(spotless, 1_000 + 80 - 1);

    for expected in [1i64, 3, 7, 15, 15, 15, 15] {
        s.note_connect_failed(P1, 0);
        assert_eq!(spotless - s.score(&c, 0), expected);
    }
}

#[test]
fn the_timeout_score_bias_expires_after_sixty_seconds() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_timeout(P1, 0);
    let c = candidate(P1, -60, 0);

    // The timeout also counted as a failure, so 2 points of penalty are present
    // throughout; the bias is the extra 10.
    assert_eq!(s.score(&c, 59_999), 1_000 + 80 - 599 - 2 - 10);
    assert_eq!(s.score(&c, 60_000), 1_000 + 80 - 600 - 2);
}

#[test]
fn busy_addresses_are_skipped_and_the_next_candidate_is_tried() {
    let mut s = ConnectionScheduler::new();
    s.enqueue(candidate("busy", -50, 1_000));
    s.enqueue(candidate("free", -70, 1_000));

    match s.next_candidate(0, |address| address == "busy", 1_000) {
        QueueDecision::Connect(c) => assert_eq!(c.address, "free"),
        other => panic!("expected a connect, got {other:?}"),
    }
    assert_eq!(
        s.candidate_count(),
        0,
        "the busy candidate is discarded too"
    );
}

#[test]
fn an_empty_queue_is_idle() {
    let mut s = ConnectionScheduler::new();
    assert_eq!(s.next_candidate(0, never_busy, 1_000), QueueDecision::Idle);
}

#[test]
fn remove_candidate_drops_a_pending_address() {
    let mut s = ConnectionScheduler::new();
    s.enqueue(candidate(P1, -50, 1_000));
    s.enqueue(candidate(P2, -50, 1_000));
    s.remove_candidate(P1);

    assert_eq!(s.candidate_count(), 1);
    match s.next_candidate(0, never_busy, 1_000) {
        QueueDecision::Connect(c) => assert_eq!(c.address, P2),
        other => panic!("expected a connect, got {other:?}"),
    }
}

// -- The moving RSSI gate --------------------------------------------------

#[test]
fn isolation_relaxes_the_gate_strictly_after_thirty_seconds() {
    let mut s = ConnectionScheduler::new();

    assert_eq!(s.update_rssi_threshold(0, 0, 100_000), RSSI_ISOLATED_BASE);
    assert_eq!(
        s.update_rssi_threshold(0, 0, 129_000),
        RSSI_ISOLATED_BASE,
        "29 s alone: still the base gate"
    );
    assert_eq!(
        s.update_rssi_threshold(0, 0, 130_000),
        RSSI_ISOLATED_BASE,
        "exactly 30 s: bitchat compares with `>`, so still base"
    );
    assert_eq!(
        s.update_rssi_threshold(0, 0, 130_001),
        RSSI_ISOLATED_RELAXED,
        "past 30 s: reach as far as the radio physically allows"
    );
}

#[test]
fn finding_a_peer_ends_isolation_and_restarts_the_clock() {
    let mut s = ConnectionScheduler::new();
    s.update_rssi_threshold(0, 0, 0);
    assert_eq!(s.isolated_for_ms(60_000), Some(60_000));

    assert_eq!(
        s.update_rssi_threshold(1, 1, 60_000),
        DYNAMIC_RSSI_THRESHOLD_DEFAULT
    );
    assert_eq!(s.isolated_for_ms(60_000), None);

    // Alone again: the relaxation clock starts over rather than resuming.
    assert_eq!(s.update_rssi_threshold(0, 0, 61_000), RSSI_ISOLATED_BASE);
    assert_eq!(
        s.update_rssi_threshold(0, 0, 91_000),
        RSSI_ISOLATED_BASE,
        "30 s after the new isolation began, not the old one"
    );
    assert_eq!(s.update_rssi_threshold(0, 0, 91_001), RSSI_ISOLATED_RELAXED);
}

#[test]
fn a_full_budget_tightens_the_gate() {
    let mut s = ConnectionScheduler::new();
    assert_eq!(
        s.update_rssi_threshold(1, 5, 0),
        DYNAMIC_RSSI_THRESHOLD_DEFAULT
    );
    assert_eq!(s.update_rssi_threshold(1, 6, 0), RSSI_CONNECTED_THRESHOLD);
    assert_eq!(s.update_rssi_threshold(1, 7, 0), RSSI_CONNECTED_THRESHOLD);
}

#[test]
fn a_full_candidate_queue_tightens_the_gate() {
    let config = SchedulerConfig {
        candidate_cap: 2,
        ..Default::default()
    };
    let mut s = ConnectionScheduler::with_config(config);
    s.enqueue(candidate(P1, -65, 0));
    assert_eq!(
        s.update_rssi_threshold(1, 1, 0),
        DYNAMIC_RSSI_THRESHOLD_DEFAULT
    );

    s.enqueue(candidate(P2, -66, 0));
    assert_eq!(s.update_rssi_threshold(1, 1, 0), RSSI_CONNECTED_THRESHOLD);
}

#[test]
fn timeouts_never_tighten_the_gate_globally() {
    // One flaky distant peer must not blind us to every other edge-of-range peer,
    // so the penalty is per-address only.
    let mut s = ConnectionScheduler::new();
    for address in ["p1", "p2", "p3"] {
        s.note_connect_timeout(address, 0);
    }
    assert_eq!(
        s.update_rssi_threshold(1, 1, 1_000),
        DYNAMIC_RSSI_THRESHOLD_DEFAULT
    );
}

#[test]
fn the_gate_moving_changes_which_adverts_are_queued() {
    let mut s = ConnectionScheduler::new();

    // Isolated and relaxed: a -97 dBm advert is now worth connecting to.
    s.update_rssi_threshold(0, 0, 0);
    assert_eq!(discover(&mut s, P1, -97, 1_000), DiscoveryDecision::Queued);

    s.update_rssi_threshold(0, 0, 31_000);
    assert_eq!(s.rssi_threshold(), RSSI_ISOLATED_RELAXED);
    assert_eq!(
        discover(&mut s, P1, -97, 32_000),
        DiscoveryDecision::ConnectNow
    );

    // Saturated: the same advert would not be worth the churn.
    s.update_rssi_threshold(1, 6, 33_000);
    assert_eq!(discover(&mut s, P1, -97, 34_000), DiscoveryDecision::Queued);
    assert_eq!(discover(&mut s, P2, -86, 34_000), DiscoveryDecision::Queued);
    assert_eq!(
        discover(&mut s, P2, -84, 34_000),
        DiscoveryDecision::ConnectNow
    );
}

// -- Housekeeping ----------------------------------------------------------

#[test]
fn prune_boundary_keeps_records_exactly_at_the_window_edge() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_timeout(P1, 1_000);
    s.note_disconnected(P2, 1_000);
    assert_eq!(s.backoff_len(), 2);

    // now - 120_000 == 1_000, and records at the cutoff are retained.
    s.prune_backoff(121_000);
    assert_eq!(s.backoff_len(), 2);

    s.prune_backoff(121_001);
    assert_eq!(s.backoff_len(), 0);
}

#[test]
fn pruning_forgets_failure_counts_for_peers_with_nothing_pending() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_failed(P1, 0);
    s.note_connect_timeout(P2, 1_000);
    s.enqueue(candidate("queued", -60, 1_000));
    s.note_connect_failed("queued", 0);

    // P1 has no timeout record and no queued candidate, so it goes; P2's record
    // is still inside the window; "queued" is still in the queue.
    s.prune_backoff(1_000);
    assert_eq!(s.failure_count(P1), 0);
    assert_eq!(s.failure_count(P2), 1);
    assert_eq!(s.failure_count("queued"), 1);

    // Past the window everything that is not queued is forgotten.
    s.prune_backoff(200_000);
    assert_eq!(s.failure_count(P2), 0);
    assert_eq!(s.failure_count("queued"), 1);
}

#[test]
fn pruning_never_underflows_near_the_unix_epoch() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_timeout(P1, 0);
    s.prune_backoff(5);
    assert_eq!(s.backoff_len(), 1);
}

#[test]
fn reset_restores_the_configured_starting_gate() {
    let config = SchedulerConfig {
        initial_rssi_threshold: -80,
        ..Default::default()
    };
    let mut s = ConnectionScheduler::with_config(config);
    s.enqueue(candidate(P1, -60, 1_000));
    s.note_connect_timeout(P1, 1_000);
    s.note_disconnected(P2, 1_000);
    s.note_connect_attempt(1_000);
    s.update_rssi_threshold(0, 0, 1_000);

    s.reset();

    assert_eq!(s.candidate_count(), 0);
    assert_eq!(s.backoff_len(), 0);
    assert_eq!(s.failure_count(P1), 0);
    assert_eq!(s.rssi_threshold(), -80);
    assert_eq!(s.isolated_for_ms(2_000), None);
    // The rate-limit clock is cleared too: the first advert after a reset
    // connects immediately.
    assert_eq!(
        discover(&mut s, P1, -50, 1_100),
        DiscoveryDecision::ConnectNow
    );
}

#[test]
fn clock_going_backwards_fails_safe_towards_waiting() {
    let mut s = ConnectionScheduler::new();
    s.note_connect_attempt(100_000);
    // An earlier `now` reads as zero elapsed, i.e. the full limit, rather than
    // wrapping into a colossal elapsed time that would disable the limit.
    assert_eq!(
        s.handle_discovery(
            &candidate(P1, -50, 90_000),
            0,
            None,
            PeripheralConnectionState::Disconnected,
            90_000
        ),
        DiscoveryDecision::ScheduleRetry { after_ms: 550 }
    );
}

// -- Convenience wrapper ---------------------------------------------------

#[test]
fn should_connect_mirrors_handle_discovery_and_still_queues() {
    let mut s = ConnectionScheduler::new();
    assert!(s.should_connect(P1, -50, 0, 1_000));
    s.note_connect_attempt(1_000);

    // Rate-limited now, but the advert is not thrown away.
    assert!(!s.should_connect(P2, -50, 0, 1_200));
    assert_eq!(s.candidate_count(), 1);
    assert!(matches!(
        s.next_candidate(1, never_busy, 1_500),
        QueueDecision::Connect(_)
    ));

    // Below the gate: queued, not connected.
    assert!(!s.should_connect(P2, -95, 0, 5_000));
    assert_eq!(s.candidate_count(), 1);
}

// -- The behaviour the whole module exists for -----------------------------

#[test]
fn a_crowded_room_produces_paced_connects_not_a_storm() {
    let mut s = ConnectionScheduler::new();
    let mut connects = 0usize;
    let mut in_flight = 0usize;

    // Thirty loud devices advertise within the same 100 ms.
    for i in 0..30 {
        let address = format!("addr-{i:02}");
        if s.should_connect(&address, -55, in_flight, 1_000 + i as u64 * 3) {
            s.note_connect_attempt(1_000 + i as u64 * 3);
            connects += 1;
            in_flight += 1;
        }
    }

    assert_eq!(connects, 1, "one attempt in the first 100 ms, not thirty");
    assert_eq!(s.candidate_count(), 29, "the rest are queued, not lost");

    // Draining at the rate limit reaches the budget and then stops.
    let mut now = 1_100;
    for _ in 0..40 {
        match s.next_candidate(in_flight, never_busy, now) {
            QueueDecision::Connect(_) => {
                s.note_connect_attempt(now);
                connects += 1;
                in_flight += 1;
            }
            QueueDecision::RetryAfter { after_ms } => now += after_ms,
            QueueDecision::Idle => break,
        }
        now += 10;
    }

    assert_eq!(in_flight, MAX_CENTRAL_LINKS);
    assert_eq!(connects, MAX_CENTRAL_LINKS);
    assert!(
        now >= 1_100 + 5 * CONNECT_RATE_LIMIT_MS,
        "five further connects cannot happen faster than the rate limit allows"
    );
}
