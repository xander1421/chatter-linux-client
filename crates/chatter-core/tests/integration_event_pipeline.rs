//! Integration tests for the event bus.
//!
//! Validates the `EventBus` publish/subscribe pipeline that carries
//! `DomainEvent`s from inbox dispatchers to subscribers (state stores + UI).

use chatter_core::domain::ids::{ChannelId, MessageId, UserId};
use chatter_core::events::EventBus;
use chatter_core::inbox::handlers::DomainEvent;

#[test]
fn test_event_bus_domain_publish_subscribe() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe_domain();

    let user_id = UserId::new();
    let channel_id = ChannelId::new();

    let sent = bus.emit_domain(DomainEvent::TypingStarted { user_id, channel_id, timestamp: 0 });
    assert!(sent > 0, "Should have at least one subscriber");

    let event = rx.try_recv().expect("Should receive the domain event");
    match event {
        DomainEvent::TypingStarted {
            user_id: recv_uid,
            channel_id: recv_cid,
            ..
        } => {
            assert_eq!(recv_uid, user_id);
            assert_eq!(recv_cid, channel_id);
        }
        other => panic!("Expected TypingStarted, got {:?}", other),
    }
}

#[test]
fn test_event_bus_multiple_subscribers() {
    let bus = EventBus::new();
    let mut rx1 = bus.subscribe_domain();
    let mut rx2 = bus.subscribe_domain();
    let mut rx3 = bus.subscribe_domain();

    let sent = bus.emit_domain(DomainEvent::TypingStopped {
        user_id: UserId::new(),
        channel_id: ChannelId::new(),
        timestamp: 0,
    });
    assert_eq!(sent, 3, "Should deliver to 3 subscribers");

    assert!(rx1.try_recv().is_ok());
    assert!(rx2.try_recv().is_ok());
    assert!(rx3.try_recv().is_ok());
}

#[test]
fn test_event_bus_ui_channel_separate() {
    let bus = EventBus::new();
    let mut domain_rx = bus.subscribe_domain();
    let mut ui_rx = bus.subscribe_ui();

    bus.emit_domain(DomainEvent::TypingStarted {
        user_id: UserId::new(),
        channel_id: ChannelId::new(),
        timestamp: 0,
    });

    assert!(domain_rx.try_recv().is_ok(), "Domain channel should receive");
    assert!(ui_rx.try_recv().is_err(), "UI channel should NOT receive domain event");
}

#[test]
fn test_event_bus_broadcast_capacity() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe_domain();

    for _ in 0..64 {
        bus.emit_domain(DomainEvent::TypingStarted {
            user_id: UserId::new(),
            channel_id: ChannelId::new(),
            timestamp: 0,
        });
    }

    let mut count = 0;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    assert_eq!(count, 64, "Should receive all 64 events within capacity");
}

#[test]
fn test_domain_event_key_variants() {
    let user_id = UserId::new();
    let channel_id = ChannelId::new();
    let message_id = MessageId::new();

    let typing = DomainEvent::TypingStarted { user_id, channel_id, timestamp: 0 };
    let _ = typing.clone();
    let _ = format!("{:?}", typing);

    let reaction = DomainEvent::ReactionAdded {
        sender_id: user_id,
        sent_at_ns: 1234567890,
        reactor_id: UserId::new(),
        emoji: "👍".to_string(),
        timestamp: 1234567890,
    };
    let _ = format!("{:?}", reaction);

    let receipt = DomainEvent::MessagesRead {
        reader_id: user_id,
        message_timestamps: vec![100, 200, 300],
        receipt_timestamp: 400,
        channel_id: Some(channel_id),
    };
    let _ = format!("{:?}", receipt);

    let pin = DomainEvent::MessagePinned {
        message_id,
        pinner_id: user_id,
        channel_id,
        target_sent_timestamp: 1234567890,
    };
    let _ = format!("{:?}", pin);
}
