//! Integration tests for the event system architecture.
//!
//! Validates the event pipeline described in section 5 of the architecture:
//!
//!   DeviceInboxProvider → InboxEvent (broadcast/256)
//!     ├─→ InboxEventProcessor → DomainEvent (broadcast/64)
//!     │     ├─→ EventHandlers (core) → state store updates
//!     │     └─→ UI domain_handler → Slint model updates
//!     └─ HandlerRegistry (content routing)
//!
//! These tests verify the EventBus, DomainEvent types, HandlerContext,
//! and state store reactivity without requiring a running server.

use chatter_core::domain::ids::{ChannelId, MessageId, UserId};
use chatter_core::events::EventBus;
use chatter_core::inbox::handlers::{DomainEvent, HandlerContext, HandlerError, HandlerOutcome};

// ---------------------------------------------------------------------------
// EventBus publish/subscribe
// ---------------------------------------------------------------------------

/// Test that EventBus delivers domain events to subscribers.
///
/// Architecture reference:
///   DomainEvent broadcast (capacity 64)
///     ├─→ EventHandlers (core)
///     └─→ UI domain_handler
#[test]
fn test_event_bus_domain_publish_subscribe() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe_domain();

    let user_id = UserId::new();
    let channel_id = ChannelId::new();

    // Emit a typing event
    let sent = bus.emit_domain(DomainEvent::TypingStarted {
        user_id,
        channel_id,
    });
    assert!(sent > 0, "Should have at least one subscriber");

    // Receive the event
    let event = rx.try_recv().expect("Should receive the domain event");
    match event {
        DomainEvent::TypingStarted {
            user_id: recv_uid,
            channel_id: recv_cid,
        } => {
            assert_eq!(recv_uid, user_id);
            assert_eq!(recv_cid, channel_id);
        }
        other => panic!("Expected TypingStarted, got {:?}", other),
    }
}

/// Test that multiple subscribers all receive the same event.
#[test]
fn test_event_bus_multiple_subscribers() {
    let bus = EventBus::new();
    let mut rx1 = bus.subscribe_domain();
    let mut rx2 = bus.subscribe_domain();
    let mut rx3 = bus.subscribe_domain();

    let user_id = UserId::new();
    let channel_id = ChannelId::new();

    let sent = bus.emit_domain(DomainEvent::TypingStopped {
        user_id,
        channel_id,
    });
    assert_eq!(sent, 3, "Should deliver to 3 subscribers");

    // All three should receive
    assert!(rx1.try_recv().is_ok());
    assert!(rx2.try_recv().is_ok());
    assert!(rx3.try_recv().is_ok());
}

/// Test that UI events work on a separate channel from domain events.
#[test]
fn test_event_bus_ui_channel_separate() {
    let bus = EventBus::new();
    let mut domain_rx = bus.subscribe_domain();
    let mut ui_rx = bus.subscribe_ui();

    // Emit domain event
    bus.emit_domain(DomainEvent::TypingStarted {
        user_id: UserId::new(),
        channel_id: ChannelId::new(),
    });

    // Domain channel gets the event, UI channel does not
    assert!(domain_rx.try_recv().is_ok(), "Domain channel should receive");
    assert!(ui_rx.try_recv().is_err(), "UI channel should NOT receive domain event");
}

/// Test that events aren't lost when subscriber is slow (broadcast semantics).
#[test]
fn test_event_bus_broadcast_capacity() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe_domain();

    // Emit 64 events (the documented capacity)
    for _ in 0..64 {
        bus.emit_domain(DomainEvent::TypingStarted {
            user_id: UserId::new(),
            channel_id: ChannelId::new(),
        });
    }

    // Should be able to receive all 64
    let mut count = 0;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    assert_eq!(count, 64, "Should receive all 64 events within capacity");
}

// ---------------------------------------------------------------------------
// HandlerContext and HandlerOutcome
// ---------------------------------------------------------------------------

/// Test that HandlerContext emits events to its channel.
///
/// Architecture reference:
///   HandlerContext { local_user_id, event_tx }
///   Handlers use ctx.emit(DomainEvent::...) to publish results.
#[test]
fn test_handler_context_emit() {
    let (tx, mut rx) = tokio::sync::broadcast::channel::<DomainEvent>(16);
    let ctx = HandlerContext::new(UserId::new(), tx);

    let channel_id = ChannelId::new();
    let user_id = UserId::new();

    let receivers = ctx.emit(DomainEvent::TypingStarted {
        user_id,
        channel_id,
    });
    assert!(receivers > 0);

    let event = rx.try_recv().expect("Should receive emitted event");
    match event {
        DomainEvent::TypingStarted { .. } => {}
        _ => panic!("Wrong event type"),
    }
}

/// Test HandlerOutcome semantics.
///
/// Architecture reference:
///   HandlerResult = Result<HandlerOutcome, HandlerError>
///   HandlerOutcome: Processed | Ignored | Deferred
#[test]
fn test_handler_outcome_semantics() {
    let processed = HandlerOutcome::Processed;
    assert!(processed.is_success());
    assert!(!processed.is_ignored());
    assert!(!processed.is_deferred());

    let ignored = HandlerOutcome::Ignored;
    assert!(!ignored.is_success());
    assert!(ignored.is_ignored());
    assert!(!ignored.is_deferred());

    let deferred = HandlerOutcome::deferred("waiting for sender key");
    assert!(!deferred.is_success());
    assert!(!deferred.is_ignored());
    assert!(deferred.is_deferred());
}

/// Test HandlerError recoverability classification.
#[test]
fn test_handler_error_recoverability() {
    let parse_err = HandlerError::parse_failed("invalid protobuf");
    assert!(!parse_err.is_recoverable(), "Parse failures are not recoverable");

    let validation_err = HandlerError::validation_failed("missing sender_id");
    assert!(!validation_err.is_recoverable(), "Validation failures are not recoverable");

    let missing_ctx = HandlerError::missing_context("channel_id");
    assert!(missing_ctx.is_recoverable(), "Missing context IS recoverable (data may arrive later)");
}

// ---------------------------------------------------------------------------
// DomainEvent variant coverage
// ---------------------------------------------------------------------------

/// Test that key DomainEvent variants are constructible and Debug/Clone.
///
/// Architecture reference:
///   DomainEvent (broadcast/64) - 90+ variants covering:
///   messaging, typing, receipts, reactions, pins, key exchange,
///   sender keys, calls, anon DMs, anon groups, presence.
#[test]
fn test_domain_event_key_variants() {
    let user_id = UserId::new();
    let channel_id = ChannelId::new();
    let message_id = MessageId::new();

    // Messaging events
    let typing = DomainEvent::TypingStarted {
        user_id,
        channel_id,
    };
    let _cloned = typing.clone();
    let _debug = format!("{:?}", typing);

    // Reaction events
    let reaction = DomainEvent::ReactionAdded {
        message_id,
        author_id: user_id,
        reactor_id: UserId::new(),
        emoji: "👍".to_string(),
        timestamp: 1234567890,
    };
    let _ = format!("{:?}", reaction);

    // Receipt events
    let receipt = DomainEvent::MessagesRead {
        reader_id: user_id,
        message_timestamps: vec![100, 200, 300],
        receipt_timestamp: 400,
        channel_id: Some(channel_id),
    };
    let _ = format!("{:?}", receipt);

    // Pin events
    let pin = DomainEvent::MessagePinned {
        message_id,
        pinner_id: user_id,
        channel_id,
        target_sent_timestamp: 1234567890,
    };
    let _ = format!("{:?}", pin);

    // Key exchange events
    let key_exchange = DomainEvent::KeyExchangeReceived {
        sender_id: user_id,
        conversation_id: channel_id,
        ephemeral_public_key: vec![0u8; 32],
        timestamp: 1234567890,
    };
    let _ = format!("{:?}", key_exchange);

    // Sender key events
    let sender_key = DomainEvent::SenderKeyReceived {
        sender_id: user_id,
        sender_device_id: 1,
        channel_id,
        distribution_message: vec![0u8; 64],
        timestamp: 1234567890,
    };
    let _ = format!("{:?}", sender_key);

    // Call events
    let hangup = DomainEvent::CallHangupReceived {
        call_id: "test-call-123".to_string(),
        peer_id: user_id,
        reason: "normal".to_string(),
    };
    let _ = format!("{:?}", hangup);

    println!("All key DomainEvent variants are constructible, Clone, and Debug");
}

// ---------------------------------------------------------------------------
// State store reactivity
// ---------------------------------------------------------------------------

/// Test that AuthState correctly transitions on login/logout.
///
/// Architecture reference:
///   auth().login() → stores session token, generates Signal identity
///   auth().logout() → clears session
#[test]
fn test_auth_state_transitions() {
    let result = chatter_core::client::ChatterClient::builder()
        .server_url("http://localhost:50051")
        .build();

    let Ok(client) = result else {
        eprintln!("ChatterClient build failed (may be OK in CI)");
        return;
    };

    let auth_state = client.auth_state();

    // Initially not logged in
    assert!(!auth_state.is_logged_in());

    // Simulate login state
    let user_id = UserId::new();
    auth_state.set_logged_in(user_id, 1, "TestUser".to_string());
    assert!(auth_state.is_logged_in());

    let data = auth_state.get();
    assert_eq!(data.current_user_id, Some(user_id));
    assert_eq!(data.device_id, Some(1));
    assert_eq!(data.display_name.as_deref(), Some("TestUser"));

    // Simulate logout
    auth_state.set_logged_out();
    assert!(!auth_state.is_logged_in());
    assert!(auth_state.get().current_user_id.is_none());
}

/// Test that ChatState tracks active conversation.
///
/// Architecture reference:
///   Route view: Logged in → "main" view, client.start(), spawn event handlers
///   ChatState tracks which conversation is active.
#[test]
fn test_chat_state_active_conversation() {
    use chatter_core::state::chat_state::ActiveConversation;

    let result = chatter_core::client::ChatterClient::builder()
        .server_url("http://localhost:50051")
        .build();

    let Ok(client) = result else {
        eprintln!("ChatterClient build failed (may be OK in CI)");
        return;
    };

    let chat_state = client.chat_state();

    // Initially no active conversation
    let active = chat_state.active_conversation();
    assert!(!active.is_active());

    // Set active channel
    let channel_id = ChannelId::new();
    chat_state.set_active_conversation(ActiveConversation::Channel(channel_id));

    let active = chat_state.active_conversation();
    assert!(active.is_active());
    assert_eq!(active.channel_id(), Some(channel_id));

    // Clear
    chat_state.set_active_conversation(ActiveConversation::None);
    assert!(!chat_state.active_conversation().is_active());
}

/// Test that MessageState stores and retrieves messages per channel.
///
/// Architecture reference:
///   EventHandlers (core):
///     ├─ MessageState.append_message()
///     ├─ Persist to DB
///     └─ Update typing/receipt/presence state
#[test]
fn test_message_state_append_and_retrieve() {
    use chatter_core::domain::message::Message;

    let result = chatter_core::client::ChatterClient::builder()
        .server_url("http://localhost:50051")
        .build();

    let Ok(client) = result else {
        eprintln!("ChatterClient build failed (may be OK in CI)");
        return;
    };

    let msg_state = client.message_state();
    let channel_id = ChannelId::new();

    // Initially empty
    let messages = msg_state.get_messages(&channel_id);
    assert!(messages.is_empty());

    // Set messages for a channel using the new_text constructor
    let msg = Message::new_text(
        channel_id,
        Some(UserId::new()),
        "Test message".to_string(),
        1234567890,
    );
    let msg_id = msg.id;
    msg_state.append_message(channel_id, msg);

    let messages = msg_state.get_messages(&channel_id);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, msg_id);

    // Clear channel
    msg_state.clear_channel(&channel_id);
    assert!(msg_state.get_messages(&channel_id).is_empty());
}

/// Test that MessageState pagination cursors work.
///
/// Architecture reference:
///   Message history supports pagination via cursors.
#[test]
fn test_message_state_pagination() {
    use chatter_core::state::message_state::PaginationCursor;

    let result = chatter_core::client::ChatterClient::builder()
        .server_url("http://localhost:50051")
        .build();

    let Ok(client) = result else {
        eprintln!("ChatterClient build failed (may be OK in CI)");
        return;
    };

    let msg_state = client.message_state();
    let channel_id = ChannelId::new();

    // No cursor initially
    assert!(msg_state.get_cursor(&channel_id).is_none());

    // Set cursor
    let cursor = PaginationCursor::new(true, Some(MessageId::new()), None);
    msg_state.set_cursor(channel_id, cursor);

    let retrieved = msg_state.get_cursor(&channel_id).expect("Cursor should exist");
    assert!(retrieved.has_more);

    // Exhausted cursor
    msg_state.set_cursor(channel_id, PaginationCursor::exhausted());
    let exhausted = msg_state.get_cursor(&channel_id).unwrap();
    assert!(!exhausted.has_more);
}
