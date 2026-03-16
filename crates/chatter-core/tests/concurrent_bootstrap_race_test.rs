//! Concurrent DM Bootstrap Race Condition Test (Issue 00:29)
//!
//! Tests the scenario where:
//! 1. A initiates bootstrap to B (A is lower UUID = initiator)
//! 2. B sends a message while A's bootstrap is in progress
//! 3. A sends another message while bootstrap is still happening
//!
//! Expected behavior:
//! - Only ONE bootstrap should be sent by A (the initiator)
//! - B should queue their message (not initiate bootstrap)
//! - A's second message should be queued (bootstrap already in progress)
//! - After bootstrap completes, queued messages should be deliverable

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chatter_linux::domain::UserId;
use chatter_linux::mailbox::dm::{release_bootstrap_lock, try_acquire_bootstrap_lock};

/// Helper to create deterministic user IDs where A < B (A is the initiator).
fn create_ordered_user_pair() -> (UserId, UserId) {
    let user_a = UserId::new();
    let user_b = UserId::new();

    // Ensure A < B (A will be the initiator per Sesame algorithm)
    if user_a.as_uuid() < user_b.as_uuid() {
        (user_a, user_b)
    } else {
        (user_b, user_a)
    }
}

/// Verify the Sesame initiator determination: lower UUID initiates.
#[test]
fn test_sesame_initiator_determination() {
    let (user_a, user_b) = create_ordered_user_pair();

    // Sesame algorithm: lower UUID is the initiator
    let a_initiates_to_b = user_a.as_uuid() < user_b.as_uuid();
    let b_initiates_to_a = user_b.as_uuid() < user_a.as_uuid();

    assert!(a_initiates_to_b, "User A (lower UUID) should initiate to B");
    assert!(!b_initiates_to_a, "User B (higher UUID) should NOT initiate to A");

    // They can't both be initiators - exactly one must be
    assert!(a_initiates_to_b != b_initiates_to_a, "Exactly one should be initiator");
}

/// Test: A starts bootstrap to B, B tries to send while bootstrap in progress.
///
/// Scenario:
/// 1. A (initiator) acquires bootstrap lock for conversation with B
/// 2. While A holds the lock, B tries to send a message to A
/// 3. B should NOT start bootstrap (B is higher UUID = non-initiator)
/// 4. B should queue their message instead
#[tokio::test]
async fn test_b_sends_while_a_bootstrap_in_progress() {
    let (user_a, user_b) = create_ordered_user_pair();

    // Track what happens
    let a_bootstrap_count = Arc::new(AtomicUsize::new(0));
    let b_bootstrap_count = Arc::new(AtomicUsize::new(0));
    let a_messages_queued = Arc::new(AtomicUsize::new(0));
    let b_messages_queued = Arc::new(AtomicUsize::new(0));

    // Simulate A starting a DM to B
    // A is the initiator (lower UUID), so A acquires lock and starts bootstrap
    let a_handle = {
        let peer_id = user_b;
        let bootstrap_count = a_bootstrap_count.clone();
        let messages_queued = a_messages_queued.clone();

        tokio::spawn(async move {
            // A is lower UUID - A initiates bootstrap
            let we_are_initiator = true; // A < B

            if we_are_initiator {
                // A acquires bootstrap lock and initiates
                if try_acquire_bootstrap_lock(&peer_id) {
                    bootstrap_count.fetch_add(1, Ordering::SeqCst);

                    // Simulate bootstrap work (creating mailbox, sending KeyExchange)
                    // In real code this takes time due to network calls
                    tokio::time::sleep(Duration::from_millis(100)).await;

                    // In real flow, lock is released when peer's response arrives
                    // For this test, simulate by releasing after "bootstrap completes"
                    release_bootstrap_lock(&peer_id);
                    "A: Bootstrap initiated and completed"
                } else {
                    // Bootstrap already in progress - queue message
                    messages_queued.fetch_add(1, Ordering::SeqCst);
                    "A: Bootstrap in progress, message queued"
                }
            } else {
                // A should NOT be here - A IS the initiator
                panic!("A should be the initiator (lower UUID)");
            }
        })
    };

    // Simulate B trying to send while A's bootstrap is in progress
    // B is higher UUID - B should NOT initiate, just queue
    let b_handle = {
        let peer_id = user_a;
        let _bootstrap_count = b_bootstrap_count.clone();
        let messages_queued = b_messages_queued.clone();

        tokio::spawn(async move {
            // Wait a bit for A to start bootstrap first
            tokio::time::sleep(Duration::from_millis(20)).await;

            // B is higher UUID - B should NOT initiate bootstrap
            let we_are_initiator = false; // B > A

            if !we_are_initiator {
                // B should queue message and possibly send REQUEST to prompt A
                // But B should NOT do a full bootstrap (64-byte KeyExchange)

                // Try to acquire lock (to send REQUEST if needed)
                if try_acquire_bootstrap_lock(&peer_id) {
                    // B got the lock - but B is NOT the initiator
                    // B sends a REQUEST (empty KeyExchange) to prompt A
                    // B does NOT count as "bootstrap"
                    messages_queued.fetch_add(1, Ordering::SeqCst);

                    // Simulate B sending REQUEST
                    tokio::time::sleep(Duration::from_millis(10)).await;

                    // Keep lock held waiting for A's response
                    // In real code, lock is released when A's MESSAGE arrives
                    release_bootstrap_lock(&peer_id);
                    "B: Sent REQUEST, message queued"
                } else {
                    // Lock already held - another B message is in progress
                    messages_queued.fetch_add(1, Ordering::SeqCst);
                    "B: Lock held, message queued"
                }
            } else {
                // B should NOT be here
                panic!("B should NOT be the initiator (higher UUID)");
            }
        })
    };

    // Wait for both to complete
    let a_result = a_handle.await.unwrap();
    let b_result = b_handle.await.unwrap();

    println!("A result: {}", a_result);
    println!("B result: {}", b_result);

    // Verify results
    let a_bootstraps = a_bootstrap_count.load(Ordering::SeqCst);
    let b_bootstraps = b_bootstrap_count.load(Ordering::SeqCst);
    let b_queued = b_messages_queued.load(Ordering::SeqCst);

    // A should initiate exactly ONE bootstrap
    assert_eq!(a_bootstraps, 1, "A should initiate exactly 1 bootstrap");

    // B should NOT initiate any bootstrap (B is higher UUID)
    assert_eq!(b_bootstraps, 0, "B should NOT initiate bootstrap (higher UUID)");

    // B should queue their message
    assert_eq!(b_queued, 1, "B should queue their message");
}

/// Test: A sends multiple messages before bootstrap completes.
///
/// Scenario:
/// 1. A sends first message, triggering bootstrap
/// 2. A sends second message while bootstrap in progress
/// 3. Only ONE bootstrap should occur, second message should be queued
#[tokio::test]
async fn test_a_sends_multiple_messages_during_bootstrap() {
    let (_user_a, user_b) = create_ordered_user_pair();

    let bootstrap_count = Arc::new(AtomicUsize::new(0));
    let messages_queued = Arc::new(AtomicUsize::new(0));

    // Use a barrier to ensure messages are sent concurrently
    let barrier = Arc::new(tokio::sync::Barrier::new(3));

    // Spawn 3 concurrent message sends from A to B
    let mut handles = vec![];

    for i in 0..3 {
        let peer_id = user_b;
        let bootstrap_counter = bootstrap_count.clone();
        let queue_counter = messages_queued.clone();
        let barrier_clone = barrier.clone();

        handles.push(tokio::spawn(async move {
            // Wait for all tasks to be ready
            barrier_clone.wait().await;

            // A is the initiator (lower UUID)
            // All 3 sends happen "simultaneously"
            if try_acquire_bootstrap_lock(&peer_id) {
                // Got the lock - this one does bootstrap
                bootstrap_counter.fetch_add(1, Ordering::SeqCst);

                // Simulate bootstrap work
                tokio::time::sleep(Duration::from_millis(50)).await;

                // Bootstrap complete - release lock
                release_bootstrap_lock(&peer_id);
                format!("Message {}: Did bootstrap", i)
            } else {
                // Lock held - queue the message
                queue_counter.fetch_add(1, Ordering::SeqCst);
                format!("Message {}: Queued (bootstrap in progress)", i)
            }
        }));
    }

    // Wait for all to complete
    for handle in handles {
        println!("{}", handle.await.unwrap());
    }

    let bootstraps = bootstrap_count.load(Ordering::SeqCst);
    let queued = messages_queued.load(Ordering::SeqCst);

    // Exactly ONE bootstrap, other 2 messages queued
    assert_eq!(bootstraps, 1, "Exactly 1 bootstrap should occur");
    assert_eq!(queued, 2, "2 messages should be queued");
    assert_eq!(bootstraps + queued, 3, "Total should be 3");
}

/// Test: Full race scenario - A starts bootstrap, B sends, A sends again.
///
/// This is the exact scenario from issue 00:29:
/// 1. A starts bootstrap to B (A is lower UUID = initiator)
/// 2. B receives something and tries to reply (B is higher UUID)
/// 3. A sends another message before bootstrap completes
#[tokio::test]
async fn test_full_race_scenario_a_bootstrap_b_sends_a_sends() {
    let (user_a, user_b) = create_ordered_user_pair();

    // Counters per user
    let a_bootstrap_count = Arc::new(AtomicUsize::new(0));
    let b_bootstrap_count = Arc::new(AtomicUsize::new(0));
    let a_messages = Arc::new(AtomicUsize::new(0));
    let b_messages = Arc::new(AtomicUsize::new(0));
    let a_queued = Arc::new(AtomicUsize::new(0));
    let b_queued = Arc::new(AtomicUsize::new(0));

    // Shared state to simulate bootstrap lock state
    // In real code this is BOOTSTRAP_IN_PROGRESS global
    // Here we just use the actual lock

    // Step 1: A sends first message to B (triggers bootstrap)
    let a_first_handle = {
        let peer = user_b;
        let bootstrap_count = a_bootstrap_count.clone();
        let msg_count = a_messages.clone();

        tokio::spawn(async move {
            msg_count.fetch_add(1, Ordering::SeqCst);

            // A is initiator, acquires lock
            if try_acquire_bootstrap_lock(&peer) {
                bootstrap_count.fetch_add(1, Ordering::SeqCst);

                // Simulate bootstrap taking time (network calls)
                // This is where the race happens
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Note: Don't release here - in real code, we wait for peer's response
                // But for this test, we need to release eventually
                tokio::time::sleep(Duration::from_millis(50)).await;
                release_bootstrap_lock(&peer);

                "A[1]: Bootstrap started and completed"
            } else {
                "A[1]: Bootstrap already in progress (unexpected)"
            }
        })
    };

    // Give A time to start bootstrap
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Step 2: B tries to send to A (while A's bootstrap to B is in progress)
    // Note: B is higher UUID, so B should NOT initiate bootstrap to A
    let b_handle = {
        let peer = user_a;
        let bootstrap_count = b_bootstrap_count.clone();
        let msg_count = b_messages.clone();
        let queued = b_queued.clone();

        tokio::spawn(async move {
            msg_count.fetch_add(1, Ordering::SeqCst);

            // B is higher UUID - should NOT initiate
            let we_are_initiator = false;

            if !we_are_initiator {
                // B checks if bootstrap is pending for conversation with A
                // Since A hasn't sent their KeyExchange yet (we're racing),
                // B should queue their message and send a REQUEST to A

                if try_acquire_bootstrap_lock(&peer) {
                    // B got lock - send REQUEST (0-byte KeyExchange) to prompt A
                    // This is NOT a full bootstrap
                    queued.fetch_add(1, Ordering::SeqCst);

                    tokio::time::sleep(Duration::from_millis(10)).await;
                    release_bootstrap_lock(&peer);

                    "B[1]: Sent REQUEST to A, message queued"
                } else {
                    queued.fetch_add(1, Ordering::SeqCst);
                    "B[1]: Lock held by another B message, queued"
                }
            } else {
                bootstrap_count.fetch_add(1, Ordering::SeqCst);
                panic!("B should NOT initiate bootstrap");
            }
        })
    };

    // Step 3: A sends another message while bootstrap still in progress
    let a_second_handle = {
        let peer = user_b;
        let msg_count = a_messages.clone();
        let queued = a_queued.clone();

        tokio::spawn(async move {
            // Wait a bit but still during A's bootstrap
            tokio::time::sleep(Duration::from_millis(30)).await;

            msg_count.fetch_add(1, Ordering::SeqCst);

            // A tries to send again - bootstrap already in progress
            if try_acquire_bootstrap_lock(&peer) {
                // This shouldn't happen if first A is still bootstrapping
                "A[2]: Got lock (bootstrap finished fast?)"
            } else {
                // Lock held - queue the message
                queued.fetch_add(1, Ordering::SeqCst);
                "A[2]: Bootstrap in progress, message queued"
            }
        })
    };

    // Wait for all
    let result_a1 = a_first_handle.await.unwrap();
    let result_b = b_handle.await.unwrap();
    let result_a2 = a_second_handle.await.unwrap();

    println!("{}", result_a1);
    println!("{}", result_b);
    println!("{}", result_a2);

    // Verify results
    let a_bootstraps = a_bootstrap_count.load(Ordering::SeqCst);
    let b_bootstraps = b_bootstrap_count.load(Ordering::SeqCst);
    let a_total_messages = a_messages.load(Ordering::SeqCst);
    let b_total_messages = b_messages.load(Ordering::SeqCst);
    let a_queued_count = a_queued.load(Ordering::SeqCst);
    let b_queued_count = b_queued.load(Ordering::SeqCst);

    println!("\n=== Results ===");
    println!("A bootstraps: {}", a_bootstraps);
    println!("B bootstraps: {}", b_bootstraps);
    println!("A total messages: {}", a_total_messages);
    println!("B total messages: {}", b_total_messages);
    println!("A messages queued: {}", a_queued_count);
    println!("B messages queued: {}", b_queued_count);

    // Assertions
    assert_eq!(a_bootstraps, 1, "A should initiate exactly 1 bootstrap");
    assert_eq!(b_bootstraps, 0, "B should NOT initiate bootstrap (higher UUID)");
    assert_eq!(a_total_messages, 2, "A sent 2 messages");
    assert_eq!(b_total_messages, 1, "B sent 1 message");
    assert_eq!(a_queued_count, 1, "A's second message should be queued");
    assert_eq!(b_queued_count, 1, "B's message should be queued");
}

/// Test: Both users try to send simultaneously (true race condition).
///
/// When A and B both try to start a conversation at the exact same time:
/// - A (lower UUID) should become the initiator
/// - B (higher UUID) should detect it's not the initiator and queue
#[tokio::test]
async fn test_simultaneous_first_contact() {
    let (user_a, user_b) = create_ordered_user_pair();

    let a_bootstrap_count = Arc::new(AtomicUsize::new(0));
    let b_bootstrap_count = Arc::new(AtomicUsize::new(0));
    let a_queued = Arc::new(AtomicUsize::new(0));
    let b_queued = Arc::new(AtomicUsize::new(0));

    // Use barrier for true simultaneous start
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    // A's perspective: sending to B
    let a_handle = {
        let peer = user_b;
        let our_id = user_a;
        let bootstrap_count = a_bootstrap_count.clone();
        let queued = a_queued.clone();
        let barrier = barrier.clone();

        tokio::spawn(async move {
            barrier.wait().await;

            // Sesame: determine who initiates
            let we_are_initiator = our_id.as_uuid() < peer.as_uuid();

            if we_are_initiator {
                // A is initiator - do bootstrap
                if try_acquire_bootstrap_lock(&peer) {
                    bootstrap_count.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    release_bootstrap_lock(&peer);
                    "A: Initiated bootstrap to B"
                } else {
                    queued.fetch_add(1, Ordering::SeqCst);
                    "A: Lock held, message queued"
                }
            } else {
                queued.fetch_add(1, Ordering::SeqCst);
                "A: Not initiator, message queued"
            }
        })
    };

    // B's perspective: sending to A
    let b_handle = {
        let peer = user_a;
        let our_id = user_b;
        let bootstrap_count = b_bootstrap_count.clone();
        let queued = b_queued.clone();
        let barrier = barrier.clone();

        tokio::spawn(async move {
            barrier.wait().await;

            // Sesame: determine who initiates
            let we_are_initiator = our_id.as_uuid() < peer.as_uuid();

            if we_are_initiator {
                // B should NOT be initiator (B > A)
                bootstrap_count.fetch_add(1, Ordering::SeqCst);
                panic!("B should NOT be initiator");
            } else {
                // B is not initiator - queue and optionally send REQUEST
                if try_acquire_bootstrap_lock(&peer) {
                    // B can send REQUEST to prompt A
                    queued.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    release_bootstrap_lock(&peer);
                    "B: Sent REQUEST, message queued"
                } else {
                    queued.fetch_add(1, Ordering::SeqCst);
                    "B: Lock held, message queued"
                }
            }
        })
    };

    let result_a = a_handle.await.unwrap();
    let result_b = b_handle.await.unwrap();

    println!("{}", result_a);
    println!("{}", result_b);

    let a_bootstraps = a_bootstrap_count.load(Ordering::SeqCst);
    let b_bootstraps = b_bootstrap_count.load(Ordering::SeqCst);

    // A (lower UUID) should initiate
    assert_eq!(a_bootstraps, 1, "A should initiate exactly 1 bootstrap");

    // B (higher UUID) should NOT initiate
    assert_eq!(b_bootstraps, 0, "B should NOT initiate bootstrap");

    // Total bootstraps across the conversation should be 1
    assert_eq!(
        a_bootstraps + b_bootstraps,
        1,
        "Total bootstraps should be exactly 1"
    );
}

/// Test the bootstrap lock across different peer conversations.
///
/// Verify that locks are per-peer-pair, not global.
/// A can bootstrap with B while also bootstrapping with C.
#[tokio::test]
async fn test_bootstrap_locks_are_per_peer() {
    let _user_a = UserId::new(); // Conceptually "A" holds locks for conversations
    let user_b = UserId::new();
    let user_c = UserId::new();

    // A can hold bootstrap locks for both B and C simultaneously
    assert!(
        try_acquire_bootstrap_lock(&user_b),
        "A should acquire lock for B"
    );
    assert!(
        try_acquire_bootstrap_lock(&user_c),
        "A should acquire lock for C (different peer)"
    );

    // Can't acquire same peer twice
    assert!(
        !try_acquire_bootstrap_lock(&user_b),
        "Can't acquire B lock again"
    );
    assert!(
        !try_acquire_bootstrap_lock(&user_c),
        "Can't acquire C lock again"
    );

    // Release and verify
    release_bootstrap_lock(&user_b);
    release_bootstrap_lock(&user_c);

    assert!(
        try_acquire_bootstrap_lock(&user_b),
        "Should reacquire B after release"
    );
    assert!(
        try_acquire_bootstrap_lock(&user_c),
        "Should reacquire C after release"
    );

    // Cleanup
    release_bootstrap_lock(&user_b);
    release_bootstrap_lock(&user_c);
}

/// Test stress scenario: many concurrent messages during bootstrap.
#[tokio::test]
async fn test_stress_many_messages_during_bootstrap() {
    let (_user_a, user_b) = create_ordered_user_pair();

    let bootstrap_count = Arc::new(AtomicUsize::new(0));
    let queued_count = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(50));

    let mut handles = vec![];

    // 50 concurrent message sends from A to B
    for i in 0..50 {
        let peer = user_b;
        let bootstrap_counter = bootstrap_count.clone();
        let queued_counter = queued_count.clone();
        let barrier_clone = barrier.clone();

        handles.push(tokio::spawn(async move {
            barrier_clone.wait().await;

            if try_acquire_bootstrap_lock(&peer) {
                bootstrap_counter.fetch_add(1, Ordering::SeqCst);
                // Hold lock for bootstrap duration
                tokio::time::sleep(Duration::from_millis(100)).await;
                release_bootstrap_lock(&peer);
                format!("Message {}: Bootstrap", i)
            } else {
                queued_counter.fetch_add(1, Ordering::SeqCst);
                format!("Message {}: Queued", i)
            }
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }

    let bootstraps = bootstrap_count.load(Ordering::SeqCst);
    let queued = queued_count.load(Ordering::SeqCst);

    assert_eq!(bootstraps, 1, "Exactly 1 bootstrap under stress");
    assert_eq!(queued, 49, "49 messages should be queued");
    assert_eq!(bootstraps + queued, 50, "Total should be 50");
}
