//! Tests for DM bootstrap lock preventing concurrent bootstrap operations.
//!
//! When a user sends multiple messages before a DM session is created,
//! only ONE bootstrap REQUEST should be sent. The bootstrap lock ensures
//! concurrent `send_dm_content` calls don't race to send multiple REQUESTs.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chatter_core::domain::UserId;
use chatter_core::mailbox::dm::{release_bootstrap_lock, try_acquire_bootstrap_lock};

/// Test that the bootstrap lock prevents concurrent acquisitions for the same peer.
#[test]
fn test_bootstrap_lock_prevents_concurrent_acquisition() {
    let peer_id = UserId::new();

    // First acquisition should succeed
    assert!(
        try_acquire_bootstrap_lock(&peer_id),
        "First lock acquisition should succeed"
    );

    // Second acquisition for same peer should fail
    assert!(
        !try_acquire_bootstrap_lock(&peer_id),
        "Second lock acquisition for same peer should fail"
    );

    // Third acquisition should also fail
    assert!(
        !try_acquire_bootstrap_lock(&peer_id),
        "Third lock acquisition for same peer should fail"
    );

    // Release the lock
    release_bootstrap_lock(&peer_id);

    // Now acquisition should succeed again
    assert!(
        try_acquire_bootstrap_lock(&peer_id),
        "Lock acquisition after release should succeed"
    );

    // Cleanup
    release_bootstrap_lock(&peer_id);
}

/// Test that locks are per-peer (different peers can acquire simultaneously).
#[test]
fn test_bootstrap_lock_is_per_peer() {
    let peer1 = UserId::new();
    let peer2 = UserId::new();
    let peer3 = UserId::new();

    // All three peers can acquire locks simultaneously
    assert!(try_acquire_bootstrap_lock(&peer1));
    assert!(try_acquire_bootstrap_lock(&peer2));
    assert!(try_acquire_bootstrap_lock(&peer3));

    // But same peer cannot acquire twice
    assert!(!try_acquire_bootstrap_lock(&peer1));
    assert!(!try_acquire_bootstrap_lock(&peer2));
    assert!(!try_acquire_bootstrap_lock(&peer3));

    // Cleanup
    release_bootstrap_lock(&peer1);
    release_bootstrap_lock(&peer2);
    release_bootstrap_lock(&peer3);
}

/// Test that releasing a lock for one peer doesn't affect others.
#[test]
fn test_bootstrap_lock_release_is_per_peer() {
    let peer1 = UserId::new();
    let peer2 = UserId::new();

    // Both acquire locks
    assert!(try_acquire_bootstrap_lock(&peer1));
    assert!(try_acquire_bootstrap_lock(&peer2));

    // Release peer1's lock
    release_bootstrap_lock(&peer1);

    // peer1 can reacquire, peer2 still locked
    assert!(try_acquire_bootstrap_lock(&peer1));
    assert!(!try_acquire_bootstrap_lock(&peer2));

    // Cleanup
    release_bootstrap_lock(&peer1);
    release_bootstrap_lock(&peer2);
}

/// Simulates multiple concurrent message sends before session creation.
///
/// This test demonstrates that when a user rapidly sends multiple messages
/// to a peer before the DM session is established, only ONE bootstrap
/// operation proceeds while others are blocked by the lock.
#[tokio::test]
async fn test_concurrent_sends_only_one_bootstrap() {
    let peer_id = UserId::new();
    let bootstrap_count = Arc::new(AtomicUsize::new(0));
    let blocked_count = Arc::new(AtomicUsize::new(0));

    // Simulate 10 concurrent "send_dm_content" calls
    let mut handles = vec![];

    for i in 0..10 {
        let peer = peer_id;
        let bootstrap_counter = bootstrap_count.clone();
        let blocked_counter = blocked_count.clone();

        let handle = tokio::spawn(async move {
            // This simulates what happens at the start of the !we_are_initiator block
            // in message_service.rs
            if try_acquire_bootstrap_lock(&peer) {
                // Got the lock - this is the one that will do bootstrap
                bootstrap_counter.fetch_add(1, Ordering::SeqCst);

                // Simulate bootstrap work taking some time
                tokio::time::sleep(Duration::from_millis(50)).await;

                // Release lock when done
                release_bootstrap_lock(&peer);

                format!("Task {} acquired lock and did bootstrap", i)
            } else {
                // Blocked - another task is handling bootstrap
                blocked_counter.fetch_add(1, Ordering::SeqCst);
                format!("Task {} was blocked (bootstrap in progress)", i)
            }
        });

        handles.push(handle);
    }

    // Wait for all tasks to complete
    for handle in handles {
        let _ = handle.await;
    }

    let bootstraps = bootstrap_count.load(Ordering::SeqCst);
    let blocked = blocked_count.load(Ordering::SeqCst);

    // EXACTLY ONE bootstrap should have occurred
    assert_eq!(
        bootstraps, 1,
        "Expected exactly 1 bootstrap, got {}",
        bootstraps
    );

    // The other 9 should have been blocked
    assert_eq!(blocked, 9, "Expected 9 blocked tasks, got {}", blocked);

    // Total should be 10
    assert_eq!(bootstraps + blocked, 10);
}

/// Test multiple rounds of concurrent sends (simulates user typing fast).
#[tokio::test]
async fn test_multiple_rounds_of_concurrent_sends() {
    let peer_id = UserId::new();

    for round in 0..3 {
        let bootstrap_count = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];

        // 5 concurrent sends per round
        for _ in 0..5 {
            let peer = peer_id;
            let counter = bootstrap_count.clone();

            handles.push(tokio::spawn(async move {
                if try_acquire_bootstrap_lock(&peer) {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    release_bootstrap_lock(&peer);
                    true
                } else {
                    false
                }
            }));
        }

        // Wait for all to complete
        for handle in handles {
            let _ = handle.await;
        }

        let bootstraps = bootstrap_count.load(Ordering::SeqCst);
        assert_eq!(
            bootstraps, 1,
            "Round {}: Expected exactly 1 bootstrap, got {}",
            round, bootstraps
        );
    }
}

/// Test that the lock works correctly across threads (not just tokio tasks).
#[test]
fn test_bootstrap_lock_thread_safety() {
    use std::thread;

    let peer_id = UserId::new();
    let bootstrap_count = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];

    for _ in 0..10 {
        let peer = peer_id;
        let counter = bootstrap_count.clone();

        handles.push(thread::spawn(move || {
            if try_acquire_bootstrap_lock(&peer) {
                counter.fetch_add(1, Ordering::SeqCst);
                // Hold the lock briefly
                thread::sleep(Duration::from_millis(10));
                release_bootstrap_lock(&peer);
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Only one thread should have acquired the lock
    assert_eq!(bootstrap_count.load(Ordering::SeqCst), 1);
}

/// Test releasing a non-existent lock doesn't panic.
#[test]
fn test_release_nonexistent_lock_is_safe() {
    let peer_id = UserId::new();

    // Release without acquiring - should not panic
    release_bootstrap_lock(&peer_id);

    // Double release - should not panic
    assert!(try_acquire_bootstrap_lock(&peer_id));
    release_bootstrap_lock(&peer_id);
    release_bootstrap_lock(&peer_id); // Extra release

    // Lock should still work
    assert!(try_acquire_bootstrap_lock(&peer_id));
    release_bootstrap_lock(&peer_id);
}

/// Stress test with many concurrent attempts using a barrier to ensure true racing.
#[tokio::test]
async fn test_bootstrap_lock_stress() {
    use tokio::sync::Barrier;

    let peer_id = UserId::new();
    let successful_locks = Arc::new(AtomicUsize::new(0));
    let blocked_locks = Arc::new(AtomicUsize::new(0));

    // Barrier ensures all 100 tasks start the lock attempt at the same time
    let barrier = Arc::new(Barrier::new(100));

    let mut handles = vec![];

    // 100 concurrent attempts
    for _ in 0..100 {
        let peer = peer_id;
        let success_counter = successful_locks.clone();
        let blocked_counter = blocked_locks.clone();
        let barrier_clone = barrier.clone();

        handles.push(tokio::spawn(async move {
            // Wait for all tasks to be ready
            barrier_clone.wait().await;

            // NOW everyone tries to acquire at the same instant
            if try_acquire_bootstrap_lock(&peer) {
                success_counter.fetch_add(1, Ordering::SeqCst);
                // Hold the lock - don't release until all tasks have tried
                tokio::time::sleep(Duration::from_millis(50)).await;
                release_bootstrap_lock(&peer);
            } else {
                blocked_counter.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }

    // Should have exactly 1 successful lock (all 100 raced, one won)
    let success = successful_locks.load(Ordering::SeqCst);
    let blocked = blocked_locks.load(Ordering::SeqCst);

    assert_eq!(success, 1, "Expected 1 successful lock, got {}", success);
    assert_eq!(blocked, 99, "Expected 99 blocked, got {}", blocked);
}

/// Test that demonstrates the scenario: user sends 3 messages before session exists.
/// Only the first message triggers bootstrap; other two just queue their messages.
#[tokio::test]
async fn test_user_sends_multiple_messages_before_session() {
    let peer_id = UserId::new();

    // Track what each "message send" does
    let bootstrap_triggered = Arc::new(AtomicUsize::new(0));
    let messages_queued = Arc::new(AtomicUsize::new(0));

    // User sends 3 messages rapidly (before session is created)
    let message_contents = vec!["Hello!", "How are you?", "Anyone there?"];

    let mut handles = vec![];

    for (i, content) in message_contents.into_iter().enumerate() {
        let peer = peer_id;
        let bootstrap_counter = bootstrap_triggered.clone();
        let queue_counter = messages_queued.clone();

        handles.push(tokio::spawn(async move {
            // This simulates the !we_are_initiator path in message_service.rs

            // Try to acquire bootstrap lock
            let acquired = try_acquire_bootstrap_lock(&peer);

            // Message is ALWAYS queued (happens regardless of lock)
            queue_counter.fetch_add(1, Ordering::SeqCst);

            if acquired {
                // We're the one doing bootstrap
                bootstrap_counter.fetch_add(1, Ordering::SeqCst);

                // Simulate bootstrap REQUEST being sent
                tokio::time::sleep(Duration::from_millis(30)).await;

                release_bootstrap_lock(&peer);
                format!("Message {}: '{}' - triggered bootstrap", i, content)
            } else {
                // Bootstrap already in progress, just queued our message
                format!(
                    "Message {}: '{}' - queued (bootstrap in progress)",
                    i, content
                )
            }
        }));
    }

    let mut results = vec![];
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    // Verify results
    let bootstraps = bootstrap_triggered.load(Ordering::SeqCst);
    let queued = messages_queued.load(Ordering::SeqCst);

    assert_eq!(bootstraps, 1, "Only ONE bootstrap should be triggered");
    assert_eq!(queued, 3, "All 3 messages should be queued");

    // Print results for clarity
    for result in results {
        println!("{}", result);
    }
}
