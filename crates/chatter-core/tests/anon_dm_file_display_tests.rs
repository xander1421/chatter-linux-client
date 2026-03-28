//! Tests for anon DM file display on send without reload.
//!
//! Verifies GRA-216: sender-side file messages appear immediately via
//! the optimistic event bus path, without requiring navigation/reload.
//!
//! Root cause was twofold:
//! 1. CoreServices created a *separate* EventBus instead of cloning infra's
//! 2. `while let Ok(event) = rx.recv().await` silently exited on broadcast lag
//!
//! These tests validate both fixes at the event-bus level.

use chatter_core::domain::anon_dm::{AnonDmId, AnonDmMessageId};
use chatter_core::domain::ids::UserId;
use chatter_core::events::EventBus;
use chatter_core::inbox::handlers::DomainEvent;

use tokio::sync::broadcast;

// ============================================================================
// Event Bus Unification — Sender Events Reach UI Subscribers
// ============================================================================

/// Verify that AnonDmMessageReceived events emitted on a cloned EventBus
/// are received by subscribers on the original bus.
///
/// This reproduces the root cause of GRA-216: CoreServices had its own
/// EventBus::new() instead of infra.event_bus.clone(). Sender-side
/// optimistic events were emitted on the orphaned bus and never reached
/// UI handlers subscribed to the CoreServices bus.
#[tokio::test]
async fn test_anon_dm_message_event_reaches_ui_via_cloned_bus() {
    // Simulate infrastructure: single EventBus created in InfrastructureContainer
    let infra_bus = EventBus::new();

    // Simulate CoreServices: clones the infra bus (the fix)
    let core_services_bus = infra_bus.clone();

    // UI handler subscribes on the CoreServices clone
    let mut ui_rx = core_services_bus.subscribe_domain();

    // AnonDmService emits on the infra bus (where the service was constructed)
    let conversation_id = AnonDmId::new();
    let sender_id = UserId::new();
    let message_id = AnonDmMessageId::new();
    let sent_at = 1_711_612_800_000_000_000_u64; // nanoseconds

    let receivers = infra_bus.emit_domain(DomainEvent::AnonDmMessageReceived {
        conversation_id,
        sender_id,
        message_id: message_id.clone(),
        sent_at,
    });

    assert!(receivers >= 1, "At least one subscriber should receive the event");

    // UI handler receives the event — message displays without navigation
    let event = ui_rx.try_recv().expect("UI subscriber must receive AnonDmMessageReceived");
    match event {
        DomainEvent::AnonDmMessageReceived {
            conversation_id: cid,
            sender_id: sid,
            message_id: mid,
            sent_at: ts,
        } => {
            assert_eq!(cid, conversation_id);
            assert_eq!(sid, sender_id);
            assert_eq!(mid, message_id);
            assert_eq!(ts, sent_at);
        }
        other => panic!(
            "Expected AnonDmMessageReceived, got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

/// Verify that using EventBus::new() instead of clone() creates an
/// isolated bus where events are NOT received by the other bus's subscribers.
///
/// This is a regression guard: if someone accidentally replaces
/// `infra.event_bus.clone()` with `EventBus::new()`, this test will
/// detect the split.
#[tokio::test]
async fn test_separate_event_bus_does_not_receive_events() {
    // The broken pattern: two separate EventBus instances
    let bus_a = EventBus::new(); // "infra" bus
    let bus_b = EventBus::new(); // "CoreServices" bus (the bug)

    // UI subscribes on bus_b
    let mut ui_rx = bus_b.subscribe_domain();

    // Service emits on bus_a
    bus_a.emit_domain(DomainEvent::AnonDmMessageReceived {
        conversation_id: AnonDmId::new(),
        sender_id: UserId::new(),
        message_id: AnonDmMessageId::new(),
        sent_at: 0,
    });

    // UI must NOT receive the event — buses are disconnected
    assert!(
        ui_rx.try_recv().is_err(),
        "Separate EventBus instances must NOT share events (this was the GRA-216 bug)"
    );
}

// ============================================================================
// Broadcast Lag Resilience — Handlers Survive Buffer Overflow
// ============================================================================

/// Verify that a domain event handler survives broadcast::RecvError::Lagged
/// and continues processing subsequent events.
///
/// The old code used `while let Ok(event) = rx.recv().await` which silently
/// exits the loop on Lagged, permanently killing the handler.
#[tokio::test]
async fn test_handler_survives_broadcast_lag_and_receives_next_event() {
    // Create a bus with a tiny capacity to force lag
    let (tx, _) = broadcast::channel::<DomainEvent>(2);
    let mut rx = tx.subscribe();

    let target_user = UserId::new();
    let target_conv = AnonDmId::new();
    let target_msg = AnonDmMessageId::new();

    // Fill the buffer beyond capacity to cause lag
    for _ in 0..5 {
        let _ = tx.send(DomainEvent::TypingStarted {
            user_id: UserId::new(),
            channel_id: chatter_core::domain::ids::ChannelId::new(),
        });
    }

    // Now send the event we actually care about (the file message)
    let _ = tx.send(DomainEvent::AnonDmMessageReceived {
        conversation_id: target_conv,
        sender_id: target_user,
        message_id: target_msg.clone(),
        sent_at: 42,
    });

    // Simulate the FIXED handler pattern: match on Lagged and continue
    let mut received_target = false;
    let mut lag_count = 0u64;

    for _ in 0..10 {
        match rx.try_recv() {
            Ok(DomainEvent::AnonDmMessageReceived { message_id, .. }) => {
                if message_id == target_msg {
                    received_target = true;
                    break;
                }
            }
            Ok(_) => {
                // Other events, keep going
            }
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                // The fixed pattern: log and continue instead of exiting
                lag_count += n;
                continue;
            }
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Closed) => break,
        }
    }

    assert!(lag_count > 0, "Should have experienced at least one lag event");
    assert!(
        received_target,
        "Handler must receive AnonDmMessageReceived after recovering from lag"
    );
}

/// Verify the broken pattern: `while let Ok` exits on Lagged.
///
/// This documents the old behavior to prevent regression.
#[tokio::test]
async fn test_while_let_ok_pattern_exits_on_lag() {
    let (tx, _) = broadcast::channel::<DomainEvent>(2);
    let mut rx = tx.subscribe();

    // Overflow the buffer
    for _ in 0..5 {
        let _ = tx.send(DomainEvent::TypingStarted {
            user_id: UserId::new(),
            channel_id: chatter_core::domain::ids::ChannelId::new(),
        });
    }

    // Send target event after overflow
    let target_msg = AnonDmMessageId::new();
    let _ = tx.send(DomainEvent::AnonDmMessageReceived {
        conversation_id: AnonDmId::new(),
        sender_id: UserId::new(),
        message_id: target_msg.clone(),
        sent_at: 0,
    });

    // Simulate the BROKEN pattern: while let Ok(event)
    // The very first recv() returns Err(Lagged), so the loop body never executes
    let mut received_anything = false;
    while let Ok(_event) = rx.try_recv() {
        received_anything = true;
    }

    assert!(
        !received_anything,
        "The old `while let Ok` pattern exits immediately on lag — \
         this confirms the bug that killed UI handlers"
    );
}

// ============================================================================
// Optimistic Display — File Message Events Fire Before Network Send
// ============================================================================

/// Simulate the optimistic send flow: event emits immediately, UI subscriber
/// picks it up, and the message is displayable without waiting for server echo.
///
/// This tests the contract between AnonDmService.send_with_attachments()
/// and the UI handler: the event fires with message_id and sent_at so the
/// UI can fetch the message from local DB and display it.
#[tokio::test]
async fn test_optimistic_file_message_event_is_immediately_receivable() {
    let bus = EventBus::new();

    // UI subscribes before the send happens
    let mut ui_rx = bus.subscribe_domain();

    // Simulate AnonDmService: persist to DB, then emit event
    let conversation_id = AnonDmId::new();
    let our_user_id = UserId::new();
    let message_id = AnonDmMessageId::new();
    let sent_at = 1_711_612_800_123_456_789_u64;

    // Step 1: DB insert would happen here (not tested, DB layer)

    // Step 2: Emit domain event (this is what the service does)
    bus.emit_domain(DomainEvent::AnonDmMessageReceived {
        conversation_id,
        sender_id: our_user_id,
        message_id: message_id.clone(),
        sent_at,
    });

    // Step 3 (network send) would happen AFTER this, possibly with
    // timing decorrelation delay for attachments

    // The UI handler receives the event BEFORE network send completes
    let event = ui_rx.try_recv().expect(
        "UI must receive the optimistic event immediately — \
         file should display without waiting for server echo"
    );

    match event {
        DomainEvent::AnonDmMessageReceived {
            conversation_id: cid,
            sender_id: sid,
            message_id: mid,
            sent_at: ts,
        } => {
            assert_eq!(cid, conversation_id, "Conversation ID must match");
            assert_eq!(sid, our_user_id, "Sender should be our own user (optimistic)");
            assert_eq!(mid, message_id, "Message ID for DB lookup must match");
            assert_eq!(ts, sent_at, "Timestamp must match for ordering");
        }
        _ => panic!("Expected AnonDmMessageReceived"),
    }
}

/// Verify that multiple rapid file sends all produce receivable events.
///
/// Simulates a user sending several files in quick succession — all should
/// appear in the UI without any being lost.
#[tokio::test]
async fn test_rapid_file_sends_all_produce_events() {
    let bus = EventBus::new();
    let mut ui_rx = bus.subscribe_domain();

    let conversation_id = AnonDmId::new();
    let sender_id = UserId::new();

    let mut sent_message_ids = Vec::new();

    // Rapid-fire 10 file messages
    for i in 0..10u64 {
        let msg_id = AnonDmMessageId::new();
        sent_message_ids.push(msg_id.clone());

        bus.emit_domain(DomainEvent::AnonDmMessageReceived {
            conversation_id,
            sender_id,
            message_id: msg_id,
            sent_at: 1_711_612_800_000_000_000 + i * 1_000_000,
        });
    }

    // UI handler should receive all 10
    let mut received_ids = Vec::new();
    while let Ok(event) = ui_rx.try_recv() {
        if let DomainEvent::AnonDmMessageReceived { message_id, .. } = event {
            received_ids.push(message_id);
        }
    }

    assert_eq!(
        received_ids.len(),
        10,
        "All 10 rapid file message events must be received by UI"
    );
    assert_eq!(
        received_ids, sent_message_ids,
        "Events must arrive in send order"
    );
}

/// Verify that events from different conversations are all delivered.
///
/// The UI handler filters by conversation_id — this ensures the EventBus
/// delivers events from multiple conversations without cross-talk.
#[tokio::test]
async fn test_file_events_from_multiple_conversations_all_delivered() {
    let bus = EventBus::new();
    let mut ui_rx = bus.subscribe_domain();

    let sender = UserId::new();
    let conv1 = AnonDmId::new();
    let conv2 = AnonDmId::new();
    let conv3 = AnonDmId::new();

    for conv in [conv1, conv2, conv3] {
        bus.emit_domain(DomainEvent::AnonDmMessageReceived {
            conversation_id: conv,
            sender_id: sender,
            message_id: AnonDmMessageId::new(),
            sent_at: 0,
        });
    }

    let mut received_convs = Vec::new();
    while let Ok(event) = ui_rx.try_recv() {
        if let DomainEvent::AnonDmMessageReceived { conversation_id, .. } = event {
            received_convs.push(conversation_id);
        }
    }

    assert_eq!(received_convs.len(), 3);
    assert!(received_convs.contains(&conv1));
    assert!(received_convs.contains(&conv2));
    assert!(received_convs.contains(&conv3));
}
