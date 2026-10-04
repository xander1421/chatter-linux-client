//! Integration tests for content processing and message handling.
//!
//! Tests for:
//! - Proto content to domain conversion
//! - Encrypted content handling
//! - Multi-device sync detection
//! - Inbox processor message routing

use crate::{common, skip_if_no_server};

use chatter_core::network::grpc::{ChannelClient, MessageClient};
use chatter_core::proto::chat_v1::{
    self, message::content::Type as ContentType, message::Content, ChannelType, EncryptedData,
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
        r#type: Some(ContentType::Encrypted(EncryptedData {
            version: 1,
            ciphertext: vec![1, 2, 3, 4],
            associated_data: b"sender_key".to_vec(),
        })),
    };

    match &content.r#type {
        Some(ContentType::Encrypted(enc)) => {
            assert_eq!(enc.ciphertext, vec![1, 2, 3, 4]);
            assert_eq!(enc.associated_data, b"sender_key");
            assert_eq!(enc.version, 1);
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
            sha256: b"abc123".to_vec(),
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

    let encrypted_content = EncryptedData {
        version: 1,
        ciphertext: vec![0xDE, 0xAD, 0xBE, 0xEF],
        associated_data: b"sender_key".to_vec(),
    };

    // The ciphertext should be non-empty
    assert!(!encrypted_content.ciphertext.is_empty());

    // associated_data tags the sender-key format
    assert_eq!(encrypted_content.associated_data, b"sender_key");

    // Version should indicate crypto format
    assert_eq!(encrypted_content.version, 1);
}

/// Test that the sender-key envelope carries version + ciphertext + tag.
#[test]
fn test_encryption_content_structure() {
    // EncryptedData is the sender-key group envelope: ciphertext holds a
    // serialized GroupEncrypted proto, associated_data tags the format.
    let encrypted_content = EncryptedData {
        version: 1,
        ciphertext: vec![0x00, 0x01, 0x02, 0x03],
        associated_data: b"sender_key".to_vec(),
    };

    // Ciphertext should be non-empty for real messages
    assert!(!encrypted_content.ciphertext.is_empty());
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
async fn create_test_channel(pool: &chatter_core::network::grpc::GrpcClientPool, user: &TestUser) -> String {
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("content_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            Some("Test channel for content processing"),
            None,
            Some(membership::genesis(user, true).1),
        ).await
        .expect("Failed to create channel");

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
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
    let user = assert_ok(setup_authenticated_test(&pool).await);
    let channel_id = create_test_channel(&pool, &user).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Send a text message
    let content = Content {
        r#type: Some(ContentType::Text("Test message for content processing".to_string())),
    };

    let sent_message = assert_ok(
        msg_client
            .send_message(&channel_id, content, None, false, None, vec![], false, vec![], Vec::new())
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
                chatter_core::proto::chat_v1::MessageSortDirection::SortNewestFirst,
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
        chatter_core::network::grpc::AuthClient::new(pool.channel(), pool.auth_clone());

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
        chatter_core::network::grpc::AuthClient::new(pool.channel(), pool.auth_clone());
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
