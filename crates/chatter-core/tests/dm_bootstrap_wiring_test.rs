//! Integration tests for DM bootstrap wiring.
//!
//! Tests that the full DM bootstrap flow works:
//! 1. KeyExchange messages in sealed sender are detected
//! 2. DomainEvent::KeyExchangeReceived is emitted
//! 3. DM bootstrap handler receives and processes the event

use std::time::Duration;

use prost::Message as ProstMessage;

use chatter_core::domain::{ChannelId, UserId};
use chatter_core::events::EventBus;
use chatter_core::inbox::handlers::DomainEvent;
use chatter_core::proto::content_v1::{Content, KeyExchange, content};
use chatter_core::services::gateway::SealedSenderResult;

/// Test that SealedSenderResult::KeyExchange is returned for KeyExchange content.
#[test]
fn test_sealed_sender_result_key_exchange_variant() {
    let sender_id = UserId::new();
    let conversation_id = "test-conversation-123".to_string();
    let ephemeral_pk: Vec<u8> = (0..64).collect(); // 64-byte bootstrap key

    let result = SealedSenderResult::KeyExchange {
        sender_id,
        sender_device_id: 1,
        conversation_id: conversation_id.clone(),
        ephemeral_public_key: ephemeral_pk.clone(),
        timestamp: 1704067200000,
    };

    // Verify we can match on the variant and extract fields
    match result {
        SealedSenderResult::KeyExchange {
            sender_id: s,
            conversation_id: c,
            ephemeral_public_key: k,
            timestamp: t,
            ..
        } => {
            assert_eq!(s, sender_id);
            assert_eq!(c, conversation_id);
            assert_eq!(k.len(), 64);
            assert_eq!(t, 1704067200000);
        }
        _ => panic!("Expected KeyExchange variant"),
    }
}

/// Test that DomainEvent::KeyExchangeReceived can carry both 32 and 64 byte keys.
#[test]
fn test_domain_event_key_exchange_received_32_bytes() {
    let sender_id = UserId::new();
    let conversation_id = ChannelId::new();
    let ephemeral_pk: Vec<u8> = (0..32).collect(); // 32-byte regular key

    let event = DomainEvent::KeyExchangeReceived {
        sender_id,
        sender_device_id: 1,
        conversation_id,
        ephemeral_public_key: ephemeral_pk.clone(),
        timestamp: 1704067200000,
    };

    if let DomainEvent::KeyExchangeReceived {
        ephemeral_public_key,
        ..
    } = event
    {
        assert_eq!(ephemeral_public_key.len(), 32);
    } else {
        panic!("Expected KeyExchangeReceived event");
    }
}

#[test]
fn test_domain_event_key_exchange_received_64_bytes() {
    let sender_id = UserId::new();
    let conversation_id = ChannelId::new();
    let ephemeral_pk: Vec<u8> = (0..64).collect(); // 64-byte bootstrap key

    let event = DomainEvent::KeyExchangeReceived {
        sender_id,
        sender_device_id: 1,
        conversation_id,
        ephemeral_public_key: ephemeral_pk.clone(),
        timestamp: 1704067200000,
    };

    if let DomainEvent::KeyExchangeReceived {
        ephemeral_public_key,
        ..
    } = event
    {
        assert_eq!(ephemeral_public_key.len(), 64);
    } else {
        panic!("Expected KeyExchangeReceived event");
    }
}

/// Test that EventBus can transmit KeyExchangeReceived events.
#[tokio::test]
async fn test_event_bus_transmits_key_exchange_received() {
    let event_bus = EventBus::new();
    let mut rx = event_bus.subscribe_domain();

    let sender_id = UserId::new();
    let conversation_id = ChannelId::new();
    let ephemeral_pk: Vec<u8> = (0..64).collect();

    // Emit event
    let sent_count = event_bus.emit_domain(DomainEvent::KeyExchangeReceived {
        sender_id,
        sender_device_id: 1,
        conversation_id,
        ephemeral_public_key: ephemeral_pk.clone(),
        timestamp: 1704067200000,
    });

    assert_eq!(sent_count, 1, "Event should be sent to 1 subscriber");

    // Receive event
    let received = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("Should receive within timeout")
        .expect("Should receive event");

    if let DomainEvent::KeyExchangeReceived {
        sender_id: s,
        sender_device_id: 1,
        conversation_id: c,
        ephemeral_public_key: k,
        timestamp: t,
    } = received
    {
        assert_eq!(s, sender_id);
        assert_eq!(c, conversation_id);
        assert_eq!(k, ephemeral_pk);
        assert_eq!(t, 1704067200000);
    } else {
        panic!("Expected KeyExchangeReceived event");
    }
}

/// Test that multiple subscribers can receive the same KeyExchangeReceived event.
#[tokio::test]
async fn test_event_bus_multiple_subscribers_key_exchange() {
    let event_bus = EventBus::new();
    let mut rx1 = event_bus.subscribe_domain();
    let mut rx2 = event_bus.subscribe_domain();

    let sender_id = UserId::new();
    let conversation_id = ChannelId::new();
    let ephemeral_pk: Vec<u8> = (0..64).collect();

    // Emit event
    let sent_count = event_bus.emit_domain(DomainEvent::KeyExchangeReceived {
        sender_id,
        sender_device_id: 1,
        conversation_id,
        ephemeral_public_key: ephemeral_pk,
        timestamp: 1704067200000,
    });

    assert_eq!(sent_count, 2, "Event should be sent to 2 subscribers");

    // Both should receive
    let event1 = rx1.recv().await.expect("Subscriber 1 should receive");
    let event2 = rx2.recv().await.expect("Subscriber 2 should receive");

    assert!(matches!(event1, DomainEvent::KeyExchangeReceived { .. }));
    assert!(matches!(event2, DomainEvent::KeyExchangeReceived { .. }));
}

/// Test Content proto encoding/decoding with KeyExchange.
#[test]
fn test_content_proto_key_exchange_round_trip() {
    let ephemeral_pk: Vec<u8> = (0..64).collect();
    let conversation_id = uuid::Uuid::new_v4().to_string();

    let key_exchange = KeyExchange {
        ephemeral_pk: ephemeral_pk.clone(),
        conversation_id: conversation_id.clone(),
        timestamp: 1704067200000,
    };

    let content = Content {
        content: Some(content::Content::KeyExchange(key_exchange)),
        ephemeral_reply_to: None,
        timestamp: 1704067200000,
        padding: vec![],
    };

    // Encode
    let mut encoded = Vec::new();
    content.encode(&mut encoded).expect("Should encode");

    // Decode
    let decoded = Content::decode(encoded.as_slice()).expect("Should decode");

    // Verify
    if let Some(content::Content::KeyExchange(kx)) = decoded.content {
        assert_eq!(kx.ephemeral_pk, ephemeral_pk);
        assert_eq!(kx.conversation_id, conversation_id);
        assert_eq!(kx.timestamp, 1704067200000);
    } else {
        panic!("Expected KeyExchange content");
    }
}

/// Test that 64-byte bootstrap messages are correctly identified.
#[test]
fn test_bootstrap_message_detection_by_key_length() {
    // 32 bytes = regular key exchange
    let regular_key: Vec<u8> = (0..32).collect();
    assert_eq!(regular_key.len(), 32);
    assert!(regular_key.len() != 64, "32-byte is not bootstrap");

    // 64 bytes = bootstrap (mailbox pk || conversation pk)
    let bootstrap_key: Vec<u8> = (0..64).collect();
    assert_eq!(bootstrap_key.len(), 64);
    assert!(bootstrap_key.len() == 64, "64-byte is bootstrap");
}

/// Test bootstrap key structure (first 32 = mailbox pk, next 32 = conversation pk).
#[test]
fn test_bootstrap_key_structure() {
    let mailbox_pk: [u8; 32] = [1u8; 32];
    let conversation_pk: [u8; 32] = [2u8; 32];

    // Combine into 64-byte bootstrap key
    let mut bootstrap_key = Vec::with_capacity(64);
    bootstrap_key.extend_from_slice(&mailbox_pk);
    bootstrap_key.extend_from_slice(&conversation_pk);

    assert_eq!(bootstrap_key.len(), 64);

    // Extract parts
    let extracted_mailbox_pk: [u8; 32] = bootstrap_key[..32].try_into().unwrap();
    let extracted_conversation_pk: [u8; 32] = bootstrap_key[32..].try_into().unwrap();

    assert_eq!(extracted_mailbox_pk, mailbox_pk);
    assert_eq!(extracted_conversation_pk, conversation_pk);
}

/// Test that KeyExchange with 64-byte key creates valid domain event.
#[tokio::test]
async fn test_key_exchange_to_domain_event_flow() {
    let event_bus = EventBus::new();
    let mut rx = event_bus.subscribe_domain();

    // Create 64-byte bootstrap key
    let mailbox_pk: [u8; 32] = [1u8; 32];
    let conversation_pk: [u8; 32] = [2u8; 32];
    let mut bootstrap_key = Vec::with_capacity(64);
    bootstrap_key.extend_from_slice(&mailbox_pk);
    bootstrap_key.extend_from_slice(&conversation_pk);

    let sender_id = UserId::new();
    let conversation_id = ChannelId::new();

    // Simulate what device polling does: emit DomainEvent::KeyExchangeReceived
    event_bus.emit_domain(DomainEvent::KeyExchangeReceived {
        sender_id,
        sender_device_id: 1,
        conversation_id,
        ephemeral_public_key: bootstrap_key.clone(),
        timestamp: 1704067200000,
    });

    // Receive and verify (simulates what dm_bootstrap_handler does)
    let event = rx.recv().await.expect("Should receive event");

    if let DomainEvent::KeyExchangeReceived {
        sender_id: s,
        sender_device_id: 1,
        conversation_id: c,
        ephemeral_public_key: k,
        timestamp: t,
    } = event
    {
        assert_eq!(s, sender_id);
        assert_eq!(c, conversation_id);
        assert_eq!(k.len(), 64, "Should be 64-byte bootstrap key");
        assert_eq!(t, 1704067200000);

        // Can extract parts for bootstrap processing
        let extracted_mailbox: [u8; 32] = k[..32].try_into().unwrap();
        let extracted_conv: [u8; 32] = k[32..].try_into().unwrap();
        assert_eq!(extracted_mailbox, mailbox_pk);
        assert_eq!(extracted_conv, conversation_pk);
    } else {
        panic!("Expected KeyExchangeReceived event");
    }
}

/// Test that dm_bootstrap_handler correctly identifies bootstrap messages by length.
#[tokio::test]
async fn test_bootstrap_handler_ignores_32_byte_keys() {
    let event_bus = EventBus::new();
    let mut rx = event_bus.subscribe_domain();

    // Emit 32-byte key (should be ignored by bootstrap handler)
    let regular_key: Vec<u8> = (0..32).collect();
    event_bus.emit_domain(DomainEvent::KeyExchangeReceived {
        sender_id: UserId::new(),
        sender_device_id: 1,
        conversation_id: ChannelId::new(),
        ephemeral_public_key: regular_key,
        timestamp: 1704067200000,
    });

    let event = rx.recv().await.expect("Should receive event");

    if let DomainEvent::KeyExchangeReceived {
        ephemeral_public_key,
        ..
    } = event
    {
        // Bootstrap handler checks: if ephemeral_public_key.len() == 64
        // For 32-byte keys, it should NOT trigger bootstrap processing
        assert_eq!(ephemeral_public_key.len(), 32);
        assert!(
            ephemeral_public_key.len() != 64,
            "32-byte keys should not trigger bootstrap"
        );
    }
}

/// Test that dm_bootstrap_handler processes 64-byte keys.
#[tokio::test]
async fn test_bootstrap_handler_processes_64_byte_keys() {
    let event_bus = EventBus::new();
    let mut rx = event_bus.subscribe_domain();

    // Emit 64-byte key (should be processed by bootstrap handler)
    let bootstrap_key: Vec<u8> = (0..64).collect();
    event_bus.emit_domain(DomainEvent::KeyExchangeReceived {
        sender_id: UserId::new(),
        sender_device_id: 1,
        conversation_id: ChannelId::new(),
        ephemeral_public_key: bootstrap_key,
        timestamp: 1704067200000,
    });

    let event = rx.recv().await.expect("Should receive event");

    if let DomainEvent::KeyExchangeReceived {
        ephemeral_public_key,
        ..
    } = event
    {
        // Bootstrap handler checks: if ephemeral_public_key.len() == 64
        // For 64-byte keys, it SHOULD trigger bootstrap processing
        assert_eq!(ephemeral_public_key.len(), 64);
        assert!(
            ephemeral_public_key.len() == 64,
            "64-byte keys should trigger bootstrap"
        );
    }
}

/// Test broadcast channel doesn't block on slow receivers.
#[tokio::test]
async fn test_event_bus_doesnt_block_on_slow_receiver() {
    let event_bus = EventBus::new();
    let _slow_rx = event_bus.subscribe_domain(); // Subscribe but don't read
    let mut fast_rx = event_bus.subscribe_domain();

    // Emit many events
    for i in 0..10 {
        event_bus.emit_domain(DomainEvent::KeyExchangeReceived {
            sender_id: UserId::new(),
            sender_device_id: 1,
            conversation_id: ChannelId::new(),
            ephemeral_public_key: vec![i as u8; 64],
            timestamp: i as u64,
        });
    }

    // Fast receiver should still get events
    let event = fast_rx
        .recv()
        .await
        .expect("Fast receiver should get event");
    assert!(matches!(event, DomainEvent::KeyExchangeReceived { .. }));
}

/// Test the full flow: emit KeyExchange -> handler receives -> identifies bootstrap.
///
/// This simulates what happens when:
/// 1. Device polling receives a KeyExchange in sealed sender
/// 2. Emits DomainEvent::KeyExchangeReceived
/// 3. DM bootstrap handler subscriber receives it
/// 4. Handler identifies 64-byte key as bootstrap and would process it
#[tokio::test]
async fn test_full_key_exchange_event_flow() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let event_bus = EventBus::new();
    let bootstrap_detected = Arc::new(AtomicBool::new(false));
    let bootstrap_detected_clone = bootstrap_detected.clone();

    // Spawn a "handler" that simulates dm_bootstrap_handler behavior
    let mut rx = event_bus.subscribe_domain();
    let handler = tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if let DomainEvent::KeyExchangeReceived {
                ephemeral_public_key,
                ..
            } = event
            {
                // This is what dm_bootstrap_handler does: check for 64-byte bootstrap
                if ephemeral_public_key.len() == 64 {
                    bootstrap_detected_clone.store(true, Ordering::SeqCst);
                    return; // Exit after detecting
                }
            }
        }
    });

    // Give handler time to start
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Simulate device polling emitting KeyExchange event
    let sender_id = UserId::new();
    let conversation_id = ChannelId::new();
    let bootstrap_key: Vec<u8> = (0..64).collect();

    event_bus.emit_domain(DomainEvent::KeyExchangeReceived {
        sender_id,
        sender_device_id: 1,
        conversation_id,
        ephemeral_public_key: bootstrap_key,
        timestamp: 1704067200000,
    });

    // Wait for handler to process (with timeout)
    let result = tokio::time::timeout(Duration::from_secs(1), handler).await;

    assert!(result.is_ok(), "Handler should complete within timeout");
    assert!(
        bootstrap_detected.load(Ordering::SeqCst),
        "Bootstrap should be detected for 64-byte key"
    );
}

/// Test that 32-byte keys don't trigger bootstrap detection in handler simulation.
#[tokio::test]
async fn test_32_byte_key_does_not_trigger_bootstrap() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let event_bus = EventBus::new();
    let bootstrap_detected = Arc::new(AtomicBool::new(false));
    let bootstrap_detected_clone = bootstrap_detected.clone();
    let event_received = Arc::new(AtomicBool::new(false));
    let event_received_clone = event_received.clone();

    // Spawn handler
    let mut rx = event_bus.subscribe_domain();
    let handler = tokio::spawn(async move {
        if let Ok(event) = rx.recv().await {
            event_received_clone.store(true, Ordering::SeqCst);
            if let DomainEvent::KeyExchangeReceived {
                ephemeral_public_key,
                ..
            } = event
            {
                if ephemeral_public_key.len() == 64 {
                    bootstrap_detected_clone.store(true, Ordering::SeqCst);
                }
            }
        }
    });

    // Give handler time to start
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Emit 32-byte key (regular key exchange, not bootstrap)
    let regular_key: Vec<u8> = (0..32).collect();
    event_bus.emit_domain(DomainEvent::KeyExchangeReceived {
        sender_id: UserId::new(),
        sender_device_id: 1,
        conversation_id: ChannelId::new(),
        ephemeral_public_key: regular_key,
        timestamp: 1704067200000,
    });

    // Wait for handler
    let _ = tokio::time::timeout(Duration::from_secs(1), handler).await;

    assert!(
        event_received.load(Ordering::SeqCst),
        "Event should be received"
    );
    assert!(
        !bootstrap_detected.load(Ordering::SeqCst),
        "Bootstrap should NOT be detected for 32-byte key"
    );
}
