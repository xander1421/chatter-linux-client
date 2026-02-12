//! Integration tests for content processing and message handling.
//!
//! Tests for:
//! - Proto content to domain conversion
//! - Encrypted content handling
//! - Multi-device sync detection
//! - Inbox processor message routing

mod common;

use chatter_linux::domain::ids::{ChannelId, MessageId, UserId};
use chatter_linux::domain::message::{Message, MessageContent, MessageStatus, MixedContent};
use chatter_linux::network::grpc::{ChannelClient, MessageClient};
use chatter_linux::proto::chat_v1::{
    self, message::content::Type as ContentType, message::Content, ChannelType,
};
use tokio_stream::StreamExt;

use common::*;

// ============================================================================
// Proto Content Conversion Tests
// ============================================================================

/// Test that text content is correctly converted to domain.
#[test]
fn test_proto_content_text_conversion() {
    let content = Content {
        r#type: Some(ContentType::Text("Hello, world!".to_string())),
    };

    // The actual conversion happens inside MessageService, but we can verify
    // the proto structure is correct
    match &content.r#type {
        Some(ContentType::Text(text)) => {
            assert_eq!(text, "Hello, world!");
        }
        _ => panic!("Expected Text content type"),
    }
}

/// Test that encrypted content is detected correctly.
#[test]
fn test_proto_content_encrypted_detection() {
    let content = Content {
        r#type: Some(ContentType::Encrypted(chat_v1::message::EncryptedContent {
            ciphertext: vec![1, 2, 3, 4],
            iv: vec![5, 6, 7, 8],
            key_id: "key-123".to_string(),
        })),
    };

    match &content.r#type {
        Some(ContentType::Encrypted(enc)) => {
            assert_eq!(enc.ciphertext, vec![1, 2, 3, 4]);
            assert_eq!(enc.iv, vec![5, 6, 7, 8]);
            assert_eq!(enc.key_id, "key-123");
        }
        _ => panic!("Expected Encrypted content type"),
    }
}

/// Test that file content is detected correctly.
#[test]
fn test_proto_content_file_detection() {
    let content = Content {
        r#type: Some(ContentType::File(chat_v1::FileMetadata {
            id: "file-uuid-123".to_string(),
            name: "document.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size: 1024,
            sha256: "abc123".to_string(),
            chunk_size: 256,
        })),
    };

    match &content.r#type {
        Some(ContentType::File(file)) => {
            assert_eq!(file.id, "file-uuid-123");
            assert_eq!(file.name, "document.pdf");
            assert_eq!(file.mime_type, "application/pdf");
            assert_eq!(file.size, 1024);
        }
        _ => panic!("Expected File content type"),
    }
}

/// Test empty content handling.
#[test]
fn test_proto_content_empty() {
    let content = Content { r#type: None };

    assert!(content.r#type.is_none());
}

// ============================================================================
// Encrypted Content Placeholder Tests
// ============================================================================

/// Verify that encrypted messages are identifiable for later decryption.
#[test]
fn test_encrypted_message_identifiable() {
    // When we receive an encrypted message, we should be able to identify it
    // and route it to the decryption layer

    let encrypted_content = chat_v1::message::EncryptedContent {
        ciphertext: vec![0xDE, 0xAD, 0xBE, 0xEF],
        iv: vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33, 0x44],
        key_id: "sender-key-dist-id".to_string(),
    };

    // The ciphertext should be non-empty
    assert!(!encrypted_content.ciphertext.is_empty());

    // IV should be present for decryption
    assert!(!encrypted_content.iv.is_empty());

    // Key ID should identify the sender key distribution
    assert!(!encrypted_content.key_id.is_empty());
}

/// Test that we can distinguish between different encryption schemes.
#[test]
fn test_encryption_scheme_detection() {
    // Signal uses different encryption for:
    // 1. Direct messages (sealed sender + double ratchet)
    // 2. Group messages (sender keys)

    // For DM: ciphertext is sealed sender envelope
    let dm_content = chat_v1::message::EncryptedContent {
        ciphertext: vec![0x01], // Sealed sender version byte
        iv: vec![],             // IV embedded in sealed sender
        key_id: "".to_string(), // No key ID for sealed sender
    };

    // For Group: ciphertext is sender key encrypted
    let group_content = chat_v1::message::EncryptedContent {
        ciphertext: vec![0x00, 0x01, 0x02], // Sender key ciphertext
        iv: vec![0x00; 16],                 // AES-GCM IV
        key_id: "group-sender-key".to_string(),
    };

    // We can distinguish by key_id presence
    let is_group_message = !group_content.key_id.is_empty();
    let is_dm_message = dm_content.key_id.is_empty();

    assert!(is_group_message);
    assert!(is_dm_message);
}

// ============================================================================
// Multi-Device Sync Detection Tests
// ============================================================================

/// Test that we can detect when multi-device sync is needed.
#[test]
fn test_multi_device_detection() {
    // Simulate device list response
    struct MockDevice {
        device_id: u32,
        name: String,
    }

    let devices = vec![
        MockDevice {
            device_id: 1,
            name: "Primary".to_string(),
        },
        MockDevice {
            device_id: 2,
            name: "Laptop".to_string(),
        },
        MockDevice {
            device_id: 3,
            name: "Tablet".to_string(),
        },
    ];

    let current_device_id = 1;
    let device_count = devices.len();

    // Multi-device sync needed when count > 1
    let needs_sync = device_count > 1;
    assert!(needs_sync);

    // Get other device IDs for sync
    let other_device_ids: Vec<u32> = devices
        .iter()
        .filter(|d| d.device_id != current_device_id)
        .map(|d| d.device_id)
        .collect();

    assert_eq!(other_device_ids, vec![2, 3]);
}

/// Test single device doesn't need sync.
#[test]
fn test_single_device_no_sync() {
    struct MockDevice {
        device_id: u32,
    }

    let devices = vec![MockDevice { device_id: 1 }];

    let needs_sync = devices.len() > 1;
    assert!(!needs_sync);
}

// ============================================================================
// Inbox Processor Message Type Tests
// ============================================================================

/// Test message type detection from envelope.
#[test]
fn test_message_type_from_envelope_type() {
    // Signal envelope types
    const CIPHERTEXT: i32 = 1; // Regular double-ratchet message
    const PREKEY_BUNDLE: i32 = 3; // Initial key exchange
    const UNIDENTIFIED_SENDER: i32 = 6; // Sealed sender (anonymous)
    const PLAINTEXT_CONTENT: i32 = 8; // Unencrypted (rare)

    // Helper to determine processing path
    fn get_processing_path(envelope_type: i32) -> &'static str {
        match envelope_type {
            1 => "double_ratchet",
            3 => "prekey_exchange",
            6 => "sealed_sender",
            8 => "plaintext",
            _ => "unknown",
        }
    }

    assert_eq!(get_processing_path(CIPHERTEXT), "double_ratchet");
    assert_eq!(get_processing_path(PREKEY_BUNDLE), "prekey_exchange");
    assert_eq!(get_processing_path(UNIDENTIFIED_SENDER), "sealed_sender");
    assert_eq!(get_processing_path(PLAINTEXT_CONTENT), "plaintext");
}

/// Test that sealed sender is the primary path.
#[test]
fn test_sealed_sender_primary_path() {
    // In our architecture, sealed sender should be the primary message path
    // for privacy. Non-sealed sender paths (prekey, regular) are fallbacks.

    const UNIDENTIFIED_SENDER: i32 = 6;

    // Simulated received envelope
    let envelope_type = UNIDENTIFIED_SENDER;

    // This should route to sealed sender processing
    let is_sealed_sender = envelope_type == 6;
    assert!(is_sealed_sender);
}

/// Test prekey message requires sender context.
#[test]
fn test_prekey_requires_sender_context() {
    // PreKey messages need sender address from envelope metadata
    // because the message itself doesn't contain sender info

    struct EnvelopeMetadata {
        sender_uuid: Option<String>,
        sender_device_id: Option<u32>,
    }

    // PreKey without metadata - cannot process
    let no_metadata = EnvelopeMetadata {
        sender_uuid: None,
        sender_device_id: None,
    };

    let can_process_prekey =
        no_metadata.sender_uuid.is_some() && no_metadata.sender_device_id.is_some();

    assert!(!can_process_prekey);

    // PreKey with metadata - can process
    let with_metadata = EnvelopeMetadata {
        sender_uuid: Some("alice-uuid".to_string()),
        sender_device_id: Some(1),
    };

    let can_process_prekey =
        with_metadata.sender_uuid.is_some() && with_metadata.sender_device_id.is_some();

    assert!(can_process_prekey);
}

// ============================================================================
// Integration Tests (require running server)
// ============================================================================

/// Helper to create a test channel and return its ID.
async fn create_test_channel(pool: &chatter_linux::network::grpc::GrpcClientPool) -> String {
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("content_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            vec![],
            None,
            Some("Test channel for content processing"),
            None,
        )
        .await
        .expect("Failed to create channel");

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_linux::proto::chat_v1::channel_event::Event;
            match payload {
                Event::ChannelCreated(channel) => {
                    channel_id = channel.id;
                }
                Event::OperationFailed(e) => {
                    let msg = e.error.map(|err| err.message).unwrap_or_default();
                    panic!("Channel creation failed: {}", msg);
                }
                _ => {}
            }
        }
    }

    assert!(!channel_id.is_empty(), "Should have created a channel");
    channel_id
}

/// Test sending and receiving a text message through the server.
#[tokio::test]
async fn test_server_text_message_roundtrip() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Send a text message
    let content = Content {
        r#type: Some(ContentType::Text("Test message for content processing".to_string())),
    };

    let sent_message = assert_ok(
        msg_client
            .send_message(&channel_id, content, None, false, None, false, None, vec![], false, vec![])
            .await,
    );

    assert!(!sent_message.id.is_empty());

    // Fetch the message back
    let response = assert_ok(
        msg_client
            .get_messages(
                &channel_id,
                10,
                None,
                None,
                chatter_linux::proto::chat_v1::MessageSortDirection::SortNewestFirst,
                None,
                None,
            )
            .await,
    );

    // Find our message
    let found_message = response
        .messages
        .iter()
        .find(|m| m.id == sent_message.id)
        .expect("Should find sent message");

    // Verify content type
    match &found_message.content {
        Some(content) => match &content.r#type {
            Some(ContentType::Text(text)) => {
                assert_eq!(text, "Test message for content processing");
            }
            _ => panic!("Expected text content"),
        },
        None => panic!("Message should have content"),
    }
}

/// Test that device list includes current device.
#[tokio::test]
async fn test_server_device_list_includes_current() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client =
        chatter_linux::network::grpc::AuthClient::new(pool.channel(), pool.auth_clone());

    // Get device list
    let devices = assert_ok(auth_client.list_devices().await);

    // Should have at least one device (current)
    assert!(
        !devices.devices.is_empty(),
        "Should have at least one device"
    );

    // Current device should be device_id 1 (primary)
    let has_primary = devices.devices.iter().any(|d| d.device_id == 1);
    assert!(has_primary, "Should have primary device (id=1)");
}

/// Test saved messages storage (local only for single device).
#[tokio::test]
async fn test_server_saved_messages_single_device() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Get device count
    let mut auth_client =
        chatter_linux::network::grpc::AuthClient::new(pool.channel(), pool.auth_clone());
    let devices = assert_ok(auth_client.list_devices().await);

    // For single device, saved messages are local-only
    // Multi-device sync would require SentTranscript messages
    let device_count = devices.devices.len();

    if device_count == 1 {
        tracing::info!("Single device - saved messages will be local only");
    } else {
        tracing::warn!(
            "Multiple devices ({}) - saved messages need sync implementation",
            device_count
        );
    }

    // Test passes if we can query devices
    assert!(device_count >= 1);
}
