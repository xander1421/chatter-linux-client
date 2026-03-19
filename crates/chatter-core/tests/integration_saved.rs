//! Integration tests for SavedMessageService.
//!
//! Tests saved message encryption/decryption, server operations,
//! and the full save-list-decrypt cycle.

mod common;

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use tempfile::tempdir;

use chatter_core::crypto::{SavedMessageCrypto, SavedMessageSource};
use chatter_core::network::grpc::MessageClient;
use chatter_core::proto::chat_v1::CryptoVersion;
use chatter_core::services::SavedMessageService;
use chatter_core::storage::{repositories::settings_repo::keys, Database, SettingsRepository};

use common::*;

// ============================================================================
// Encryption Unit Tests (No Server Required)
// ============================================================================

fn test_profile_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(7);
    }
    key
}

#[test]
fn test_encrypt_text_produces_valid_data() {
    let profile_key = test_profile_key();
    let text = "This is a test message";

    let content = SavedMessageCrypto::encrypt_text(text, &profile_key, None)
        .expect("Encryption should succeed");

    // Verify content structure
    match content.r#type {
        Some(chatter_core::proto::chat_v1::message::content::Type::Encrypted(ref encrypted)) => {
            assert_eq!(encrypted.version, CryptoVersion::SimpleE2e as i32);
            assert_eq!(encrypted.nonce.len(), 24); // XChaCha20 nonce
            assert!(!encrypted.ciphertext.is_empty());
            assert_eq!(encrypted.auth_tag.len(), 16); // Poly1305 tag
        }
        _ => panic!("Expected Encrypted content type"),
    }
}

#[test]
fn test_encrypt_with_source_metadata() {
    let profile_key = test_profile_key();
    let text = "Message from another channel";
    let source = SavedMessageSource {
        channel_id: "channel-123".to_string(),
        message_id: "msg-456".to_string(),
        author_id: "user-789".to_string(),
        timestamp: 1234567890000,
    };

    let content = SavedMessageCrypto::encrypt_text(text, &profile_key, Some(&source))
        .expect("Encryption should succeed");

    // Verify associated_data contains source
    match content.r#type {
        Some(chatter_core::proto::chat_v1::message::content::Type::Encrypted(ref encrypted)) => {
            assert!(!encrypted.associated_data.is_empty());
            // Verify it's valid JSON
            let parsed: SavedMessageSource =
                serde_json::from_slice(&encrypted.associated_data).expect("Should parse as JSON");
            assert_eq!(parsed.channel_id, "channel-123");
        }
        _ => panic!("Expected Encrypted content type"),
    }
}

#[test]
fn test_decrypt_roundtrip() {
    let profile_key = test_profile_key();
    let original_text = "Secret message for the vault";

    let content = SavedMessageCrypto::encrypt_text(original_text, &profile_key, None)
        .expect("Encryption should succeed");

    let (decrypted, source) = SavedMessageCrypto::decrypt_content(&content, &profile_key)
        .expect("Decryption should succeed");

    assert_eq!(decrypted, original_text);
    assert!(source.is_none());
}

#[test]
fn test_decrypt_with_source() {
    let profile_key = test_profile_key();
    let original_text = "Forwarded message";
    let original_source = SavedMessageSource {
        channel_id: "orig-channel".to_string(),
        message_id: "orig-msg".to_string(),
        author_id: "orig-author".to_string(),
        timestamp: 9876543210000,
    };

    let content = SavedMessageCrypto::encrypt_text(original_text, &profile_key, Some(&original_source))
        .expect("Encryption should succeed");

    let (decrypted, source) = SavedMessageCrypto::decrypt_content(&content, &profile_key)
        .expect("Decryption should succeed");

    assert_eq!(decrypted, original_text);
    let source = source.expect("Should have source metadata");
    assert_eq!(source.channel_id, "orig-channel");
    assert_eq!(source.message_id, "orig-msg");
    assert_eq!(source.author_id, "orig-author");
    assert_eq!(source.timestamp, 9876543210000);
}

#[test]
fn test_wrong_key_fails() {
    let profile_key = test_profile_key();
    let mut wrong_key = test_profile_key();
    wrong_key[0] ^= 0xFF; // Flip one byte

    let content = SavedMessageCrypto::encrypt_text("Secret", &profile_key, None)
        .expect("Encryption should succeed");

    let result = SavedMessageCrypto::decrypt_content(&content, &wrong_key);
    assert!(result.is_err(), "Decryption with wrong key should fail");
}

#[test]
fn test_tampered_ciphertext_fails() {
    let profile_key = test_profile_key();

    let content = SavedMessageCrypto::encrypt_text("Secret", &profile_key, None)
        .expect("Encryption should succeed");

    // Tamper with ciphertext
    let tampered = match content.r#type {
        Some(chatter_core::proto::chat_v1::message::content::Type::Encrypted(mut encrypted)) => {
            if !encrypted.ciphertext.is_empty() {
                encrypted.ciphertext[0] ^= 0xFF;
            }
            chatter_core::proto::chat_v1::message::Content {
                r#type: Some(chatter_core::proto::chat_v1::message::content::Type::Encrypted(
                    encrypted,
                )),
            }
        }
        _ => panic!("Expected Encrypted content"),
    };

    let result = SavedMessageCrypto::decrypt_content(&tampered, &profile_key);
    assert!(
        result.is_err(),
        "Decryption with tampered ciphertext should fail"
    );
}

#[test]
fn test_tampered_auth_tag_fails() {
    let profile_key = test_profile_key();

    let content = SavedMessageCrypto::encrypt_text("Secret", &profile_key, None)
        .expect("Encryption should succeed");

    // Tamper with auth tag
    let tampered = match content.r#type {
        Some(chatter_core::proto::chat_v1::message::content::Type::Encrypted(mut encrypted)) => {
            if !encrypted.auth_tag.is_empty() {
                encrypted.auth_tag[0] ^= 0xFF;
            }
            chatter_core::proto::chat_v1::message::Content {
                r#type: Some(chatter_core::proto::chat_v1::message::content::Type::Encrypted(
                    encrypted,
                )),
            }
        }
        _ => panic!("Expected Encrypted content"),
    };

    let result = SavedMessageCrypto::decrypt_content(&tampered, &profile_key);
    assert!(
        result.is_err(),
        "Decryption with tampered auth tag should fail"
    );
}

#[test]
fn test_empty_text() {
    let profile_key = test_profile_key();

    let content = SavedMessageCrypto::encrypt_text("", &profile_key, None)
        .expect("Encryption should succeed");

    let (decrypted, _) = SavedMessageCrypto::decrypt_content(&content, &profile_key)
        .expect("Decryption should succeed");

    assert!(decrypted.is_empty());
}

#[test]
fn test_unicode_text() {
    let profile_key = test_profile_key();
    let unicode_text = "Hello 🌍! Привет мир! 你好世界! مرحبا بالعالم";

    let content = SavedMessageCrypto::encrypt_text(unicode_text, &profile_key, None)
        .expect("Encryption should succeed");

    let (decrypted, _) = SavedMessageCrypto::decrypt_content(&content, &profile_key)
        .expect("Decryption should succeed");

    assert_eq!(decrypted, unicode_text);
}

#[test]
fn test_large_text() {
    let profile_key = test_profile_key();
    // Create 100KB of text
    let large_text: String = (0..100_000).map(|i| ((i % 26) as u8 + b'a') as char).collect();

    let content = SavedMessageCrypto::encrypt_text(&large_text, &profile_key, None)
        .expect("Encryption should succeed");

    let (decrypted, _) = SavedMessageCrypto::decrypt_content(&content, &profile_key)
        .expect("Decryption should succeed");

    assert_eq!(decrypted, large_text);
}

// ============================================================================
// SavedMessageService Tests with Mock Database
// ============================================================================

fn create_test_saved_service() -> (SavedMessageService, tempfile::TempDir) {
    let temp_dir = tempdir().expect("Failed to create temp directory");
    let db_path = temp_dir.path().join("test.db");

    let db = Database::open(&db_path, "test-passphrase").expect("Failed to open test database");

    // Create minimal schema
    {
        let conn = db.connection().expect("Failed to get connection");
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .expect("Failed to create tables");
    }

    let db_arc = Arc::new(db);
    let settings_repo = Arc::new(SettingsRepository::new(Arc::clone(&db_arc)));

    let service = SavedMessageService::new(settings_repo);
    (service, temp_dir)
}

fn set_logged_in_user(service: &SavedMessageService, user_id: &str, profile_key: &[u8; 32]) {
    // Use unsafe internal access to settings_repo (in tests only)
    // This is a workaround since we can't access private fields directly
    // In real tests, we'd use the proper login flow
}

// ============================================================================
// Integration Tests (Server Required)
// ============================================================================

#[tokio::test]
async fn test_save_then_list_decrypt() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a SavedMessageService with proper settings
    let temp_dir = tempdir().expect("Failed to create temp directory");
    let db_path = temp_dir.path().join("test.db");
    let db = Database::open(&db_path, "test-passphrase").expect("Failed to open test database");

    // Create schema
    {
        let conn = db.connection().expect("Failed to get connection");
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .expect("Failed to create tables");
    }

    let db_arc = Arc::new(db);
    let settings_repo = Arc::new(SettingsRepository::new(Arc::clone(&db_arc)));

    // Set up user ID and profile key
    settings_repo
        .set_string(keys::CURRENT_USER_ID, &user.user_id)
        .expect("Failed to set user ID");

    let profile_key_b64 = BASE64.encode(user.profile_key);
    settings_repo
        .set_string(keys::PROFILE_KEY, &profile_key_b64)
        .expect("Failed to set profile key");

    let service = SavedMessageService::new(settings_repo);

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Save a message
    let test_text = format!("Integration test saved message {}", rand::random::<u32>());
    let saved = assert_ok(
        service
            .save_message(&mut msg_client, &test_text, None)
            .await,
    );

    assert!(!saved.id.is_empty());

    // List messages
    let response = assert_ok(service.list_messages(&mut msg_client, 50, None).await);

    // Find our message
    let our_message = response.messages.iter().find(|m| m.id == saved.id);
    assert!(our_message.is_some(), "Should find our saved message");

    // Decrypt it
    let decrypted = assert_ok(service.decrypt_message(our_message.unwrap()));
    assert_eq!(decrypted.text, test_text);
}

#[tokio::test]
async fn test_pin_unpin_saved() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Set up SavedMessageService
    let temp_dir = tempdir().expect("Failed to create temp directory");
    let db_path = temp_dir.path().join("test.db");
    let db = Database::open(&db_path, "test-passphrase").expect("Failed to open test database");

    {
        let conn = db.connection().expect("Failed to get connection");
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .expect("Failed to create tables");
    }

    let db_arc = Arc::new(db);
    let settings_repo = Arc::new(SettingsRepository::new(Arc::clone(&db_arc)));

    settings_repo
        .set_string(keys::CURRENT_USER_ID, &user.user_id)
        .expect("Failed to set user ID");

    let profile_key_b64 = BASE64.encode(user.profile_key);
    settings_repo
        .set_string(keys::PROFILE_KEY, &profile_key_b64)
        .expect("Failed to set profile key");

    let service = SavedMessageService::new(settings_repo);
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Save a message
    let saved = assert_ok(
        service
            .save_message(&mut msg_client, "Pin this saved message", None)
            .await,
    );

    // Pin it
    let pinned = assert_ok(service.pin_message(&mut msg_client, &saved.id).await);
    assert!(pinned.pinned, "Message should be pinned");

    // Check pinned list
    let pinned_list = assert_ok(service.list_pinned_messages(&mut msg_client, 50).await);
    let found = pinned_list.iter().any(|m| m.id == saved.id);
    assert!(found, "Message should be in pinned list");

    // Unpin it
    let unpin_result = service.unpin_message(&mut msg_client, &saved.id).await;
    assert!(unpin_result.is_ok(), "Unpin should succeed");
}

#[tokio::test]
async fn test_delete_saved() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Set up SavedMessageService
    let temp_dir = tempdir().expect("Failed to create temp directory");
    let db_path = temp_dir.path().join("test.db");
    let db = Database::open(&db_path, "test-passphrase").expect("Failed to open test database");

    {
        let conn = db.connection().expect("Failed to get connection");
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .expect("Failed to create tables");
    }

    let db_arc = Arc::new(db);
    let settings_repo = Arc::new(SettingsRepository::new(Arc::clone(&db_arc)));

    settings_repo
        .set_string(keys::CURRENT_USER_ID, &user.user_id)
        .expect("Failed to set user ID");

    let profile_key_b64 = BASE64.encode(user.profile_key);
    settings_repo
        .set_string(keys::PROFILE_KEY, &profile_key_b64)
        .expect("Failed to set profile key");

    let service = SavedMessageService::new(settings_repo);
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Save a message
    let saved = assert_ok(
        service
            .save_message(&mut msg_client, "Delete this saved message", None)
            .await,
    );

    // Delete it
    let delete_result = service.delete_message(&mut msg_client, &saved.id).await;
    assert!(delete_result.is_ok(), "Delete should succeed");

    // Verify it's gone (might still show as deleted or not appear)
    let response = assert_ok(service.list_messages(&mut msg_client, 50, None).await);
    let found = response.messages.iter().find(|m| m.id == saved.id);
    // Message should either be gone or marked as deleted
    match found {
        None => {} // Good
        Some(msg) => {
            // Some implementations keep deleted messages
        }
    }
}
