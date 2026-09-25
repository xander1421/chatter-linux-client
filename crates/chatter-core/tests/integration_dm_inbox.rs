//! Integration tests for DM Inbox Polling.
//!
//! Tests the two-way anonymous DM mailbox system including:
//! - DM mailbox polling integration with DeviceInboxProvider
//! - Message decryption with shared secrets
//! - Event emission for UI updates

mod common;

use std::sync::Arc;
use tempfile::TempDir;

use chatter_core::crypto::dm_crypto::{decrypt_dm_message, encrypt_dm_message};
use chatter_core::domain::UserId;
use chatter_core::inbox::{InboxConfig, InboxEvent};
use chatter_core::mailbox::dm::{
    ConversationKey, DmConversation, DmConversationManager, DmMailbox, PeerMailbox, TwoWayAnonymity,
};
use chatter_core::storage::Database;
use chrono::{Duration, Utc};
use rusqlite;

// ============================================================================
// DM Crypto Tests
// ============================================================================

#[test]
fn test_dm_encrypt_decrypt_roundtrip() {
    let shared_secret = [0x42u8; 32];
    let message = "Hello, this is a secret DM!";

    let encrypted = encrypt_dm_message(message, &shared_secret).unwrap();

    // Verify encrypted is different from plaintext
    assert_ne!(encrypted, message.as_bytes());

    // Verify we can decrypt
    let decrypted = decrypt_dm_message(&encrypted, &shared_secret).unwrap();
    assert_eq!(decrypted, message);
}

#[test]
fn test_dm_encrypt_produces_different_ciphertexts() {
    let shared_secret = [0x42u8; 32];
    let message = "Same message";

    let encrypted1 = encrypt_dm_message(message, &shared_secret).unwrap();
    let encrypted2 = encrypt_dm_message(message, &shared_secret).unwrap();

    // Different random nonces should produce different ciphertexts
    assert_ne!(encrypted1, encrypted2);

    // But both should decrypt to the same message
    assert_eq!(
        decrypt_dm_message(&encrypted1, &shared_secret).unwrap(),
        message
    );
    assert_eq!(
        decrypt_dm_message(&encrypted2, &shared_secret).unwrap(),
        message
    );
}

#[test]
fn test_dm_wrong_key_fails() {
    let shared_secret1 = [0x42u8; 32];
    let shared_secret2 = [0x43u8; 32];
    let message = "Secret message";

    let encrypted = encrypt_dm_message(message, &shared_secret1).unwrap();

    // Decrypting with wrong key should fail
    let result = decrypt_dm_message(&encrypted, &shared_secret2);
    assert!(result.is_err());
}

#[test]
fn test_dm_tampered_ciphertext_fails() {
    let shared_secret = [0x42u8; 32];
    let message = "Secret message";

    let mut encrypted = encrypt_dm_message(message, &shared_secret).unwrap();

    // Tamper with the ciphertext (after the 24-byte nonce)
    if encrypted.len() > 30 {
        encrypted[30] ^= 0xFF;
    }

    // Should fail authentication
    let result = decrypt_dm_message(&encrypted, &shared_secret);
    assert!(result.is_err());
}

#[test]
fn test_dm_empty_message() {
    let shared_secret = [0x42u8; 32];
    let message = "";

    let encrypted = encrypt_dm_message(message, &shared_secret).unwrap();
    let decrypted = decrypt_dm_message(&encrypted, &shared_secret).unwrap();

    assert_eq!(decrypted, message);
}

#[test]
fn test_dm_unicode_message() {
    let shared_secret = [0x42u8; 32];
    let message = "Hello 世界! 🚀 Привет";

    let encrypted = encrypt_dm_message(message, &shared_secret).unwrap();
    let decrypted = decrypt_dm_message(&encrypted, &shared_secret).unwrap();

    assert_eq!(decrypted, message);
}

// ============================================================================
// Conversation Key Tests
// ============================================================================

#[test]
fn test_conversation_key_x25519_key_exchange() {
    // Simulate Alice and Bob key exchange
    let mut alice_key = ConversationKey::new();
    let mut bob_key = ConversationKey::new();

    // Exchange public keys
    let alice_public = alice_key.our_ephemeral_public;
    let bob_public = bob_key.our_ephemeral_public;

    // Derive shared secrets
    alice_key.derive_shared_secret(bob_public).unwrap();
    bob_key.derive_shared_secret(alice_public).unwrap();

    // Both should derive the same shared secret
    assert!(alice_key.is_established());
    assert!(bob_key.is_established());
    assert_eq!(alice_key.shared_secret, bob_key.shared_secret);
    assert!(alice_key.established_at.is_some());

    // Test encryption with derived shared secrets
    let alice_secret = alice_key.get_shared_secret().unwrap();
    let bob_secret = bob_key.get_shared_secret().unwrap();

    let message = "Secret message from Alice to Bob";
    let encrypted = encrypt_dm_message(message, alice_secret).unwrap();
    let decrypted = decrypt_dm_message(&encrypted, bob_secret).unwrap();

    assert_eq!(decrypted, message);
}

// ============================================================================
// DM Conversation State Tests
// ============================================================================

#[test]
fn test_dm_conversation_new() {
    let peer_id = UserId::new();
    let conv = DmConversation::new(peer_id);

    assert_eq!(conv.peer_id, peer_id);
    assert!(conv.our_mailbox.is_none());
    assert!(conv.peer_mailbox.is_none());
    assert!(conv.conversation_key.is_none());
    assert!(conv.needs_bootstrap());
    assert!(!conv.is_ready_to_send());
    assert!(!conv.is_ready_to_receive());
}

#[test]
fn test_dm_conversation_ready_states() {
    let peer_id = UserId::new();
    let mut conv = DmConversation::new(peer_id);

    // Add our mailbox
    conv.our_mailbox = Some(DmMailbox {
        id: "test".to_string(),
        public_key: [0u8; 32],
        private_key: [0u8; 64],
        signature: vec![],
        randomizer: None,
        token_id: "token".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(7),
        last_polled_at: None,
    });

    assert!(!conv.needs_bootstrap());
    assert!(conv.is_ready_to_receive());
    assert!(!conv.is_ready_to_send()); // Still need peer mailbox and key

    // Add peer mailbox
    conv.peer_mailbox = Some(PeerMailbox::new([0u8; 32], None));
    assert!(!conv.is_ready_to_send()); // Still need conversation key

    // Add conversation key without shared secret
    conv.conversation_key = Some(ConversationKey::new());
    assert!(!conv.is_ready_to_send()); // Need shared secret

    // Establish shared secret
    let mut key = conv.conversation_key.take().unwrap();
    key.derive_shared_secret([1u8; 32]).unwrap();
    conv.conversation_key = Some(key);

    assert!(conv.is_ready_to_send());
}

// ============================================================================
// DM Conversation Manager Tests
// ============================================================================

fn setup_test_db() -> (TempDir, Arc<Database>) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let db = Database::open(&db_path, "test_passphrase").unwrap();
    db.run_migrations().unwrap();
    (temp_dir, Arc::new(db))
}

/// Create a test mailbox with all required parent records.
fn create_test_mailbox(db: &Database, _peer_id: &UserId, mailbox_id: &str, token_id: &str) -> DmMailbox {
    let conn = db.connection().unwrap();

    // Create server key first (parent of mailbox_tokens)
    let server_key_id = format!("server-key-{}", uuid::Uuid::new_v4());
    let valid_until = (Utc::now() + Duration::days(30)).timestamp();
    conn.execute(
        "INSERT OR IGNORE INTO mailbox_server_keys (key_id, public_key_der, valid_until, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            server_key_id,
            vec![0u8; 256], // Fake RSA public key DER
            valid_until,
            Utc::now().to_rfc3339(),
        ],
    ).unwrap();

    // Create unique ephemeral keys for the token
    let mut ephemeral_public = [0u8; 32];
    rand::fill(&mut ephemeral_public);

    // Create mailbox token (parent of dm_mailboxes)
    // Schema: id, ephemeral_public_key, ephemeral_private_key, signature, message_randomizer, server_key_id, created_at, expires_at, used_at
    conn.execute(
        "INSERT INTO mailbox_tokens (id, ephemeral_public_key, ephemeral_private_key, signature, server_key_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            token_id,
            ephemeral_public.to_vec(),
            vec![0u8; 64],  // Fake private key
            vec![0u8; 256], // Fake signature
            server_key_id,
            Utc::now().to_rfc3339(),
        ],
    ).unwrap();

    DmMailbox {
        id: mailbox_id.to_string(),
        public_key: [1u8; 32],
        private_key: [2u8; 64],
        signature: vec![3, 4, 5],
        randomizer: Some(vec![6, 7, 8]),
        token_id: token_id.to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(7),
        last_polled_at: None,
    }
}

#[test]
fn test_dm_conversation_manager_save_load() {
    let (_temp_dir, db) = setup_test_db();
    let manager = DmConversationManager::new(db.clone());

    let peer_id = UserId::new();

    // Initially no conversation
    let loaded = manager.load_conversation(&peer_id).unwrap();
    assert!(loaded.is_none());

    // Save our mailbox (with required parent records)
    let mailbox = create_test_mailbox(&db, &peer_id, "mailbox-123", "token-abc");
    manager.save_our_mailbox(&peer_id, &mailbox).unwrap();

    // Save peer mailbox
    let peer_mailbox = PeerMailbox::new([9u8; 32], Some(Utc::now() + Duration::days(7)));
    manager.save_peer_mailbox(&peer_id, &peer_mailbox).unwrap();

    // Save conversation key
    let mut conv_key = ConversationKey::new();
    conv_key.derive_shared_secret([10u8; 32]).unwrap();
    manager.save_conversation_key(&peer_id, &conv_key).unwrap();

    // Load and verify
    let loaded = manager.load_conversation(&peer_id).unwrap().unwrap();
    assert_eq!(loaded.peer_id, peer_id);
    assert!(loaded.our_mailbox.is_some());
    assert!(loaded.peer_mailbox.is_some());
    assert!(loaded.conversation_key.is_some());
    assert!(loaded.is_ready_to_send());
    assert!(loaded.is_ready_to_receive());

    // Verify our mailbox details
    let our_mb = loaded.our_mailbox.unwrap();
    assert_eq!(our_mb.id, "mailbox-123");
    assert_eq!(our_mb.public_key, [1u8; 32]);
    assert_eq!(our_mb.token_id, "token-abc");
}

#[test]
fn test_dm_conversation_manager_get_active_mailbox_keys() {
    let (_temp_dir, db) = setup_test_db();
    let manager = DmConversationManager::new(db.clone());

    // Initially no active mailboxes
    let active = manager.get_active_mailbox_keys().unwrap();
    assert!(active.is_empty());

    // Add two active mailboxes
    let peer1 = UserId::new();
    let peer2 = UserId::new();

    let mailbox1 = create_test_mailbox(&db, &peer1, "mb-1", "t1");
    let mailbox2 = create_test_mailbox(&db, &peer2, "mb-2", "t2");

    manager.save_our_mailbox(&peer1, &mailbox1).unwrap();
    manager.save_our_mailbox(&peer2, &mailbox2).unwrap();

    // Should get both
    let active = manager.get_active_mailbox_keys().unwrap();
    assert_eq!(active.len(), 2);
}

#[test]
fn test_dm_conversation_manager_cleanup_expired() {
    let (_temp_dir, db) = setup_test_db();
    let manager = DmConversationManager::new(db.clone());

    // Add an expired mailbox
    let peer_id = UserId::new();
    let mut expired_mailbox = create_test_mailbox(&db, &peer_id, "expired", "t1");
    expired_mailbox.created_at = Utc::now() - Duration::days(14);
    expired_mailbox.expires_at = Utc::now() - Duration::days(7); // Expired!
    manager.save_our_mailbox(&peer_id, &expired_mailbox).unwrap();

    // Cleanup should remove it
    let cleaned = manager.cleanup_expired().unwrap();
    assert_eq!(cleaned, 1);

    // Verify it's gone
    let loaded = manager.load_conversation(&peer_id).unwrap();
    assert!(loaded.is_none() || loaded.unwrap().our_mailbox.is_none());
}

// ============================================================================
// InboxEvent Tests
// ============================================================================

#[test]
fn test_inbox_event_dm_received_format() {
    let peer_id = UserId::new();
    let event = InboxEvent::DmReceived {
        peer_id,
        text: "Hello!".to_string(),
        timestamp: 1234567890,
        message_id: "msg-123".to_string(),
        attachments: vec![],
    };

    // Verify event can be cloned and debugged
    let cloned = event.clone();
    let debug_str = format!("{:?}", cloned);
    assert!(debug_str.contains("DmReceived"));
    assert!(debug_str.contains("Hello!"));
}

#[test]
fn test_inbox_event_dm_decryption_failed_format() {
    let peer_id = UserId::new();
    let event = InboxEvent::DmDecryptionFailed {
        peer_id,
        error: "No shared secret".to_string(),
        convergence_failed: false,
    };

    let debug_str = format!("{:?}", event);
    assert!(debug_str.contains("DmDecryptionFailed"));
    assert!(debug_str.contains("No shared secret"));
}

// ============================================================================
// TwoWayAnonymity Unit Tests
// ============================================================================

#[test]
fn test_two_way_anonymity_creation() {
    let (_temp_dir, db) = setup_test_db();
    let _two_way = TwoWayAnonymity::new(db);
    // Just verify it can be created
}

#[test]
fn test_two_way_anonymity_can_send_receive() {
    let (_temp_dir, db) = setup_test_db();
    let two_way = TwoWayAnonymity::new(db.clone());
    let manager = DmConversationManager::new(db.clone());

    let peer_id = UserId::new();

    // Initially can't send or receive
    assert!(!two_way.can_send(&peer_id).unwrap());
    assert!(!two_way.can_receive(&peer_id).unwrap());

    // Add mailboxes and key
    let mailbox = create_test_mailbox(&db, &peer_id, "mb", "t");
    manager.save_our_mailbox(&peer_id, &mailbox).unwrap();
    manager.save_peer_mailbox(&peer_id, &PeerMailbox::new([9u8; 32], None)).unwrap();

    let mut key = ConversationKey::new();
    key.derive_shared_secret([10u8; 32]).unwrap();
    manager.save_conversation_key(&peer_id, &key).unwrap();

    // Now should be able to send and receive
    assert!(two_way.can_send(&peer_id).unwrap());
    assert!(two_way.can_receive(&peer_id).unwrap());
}
