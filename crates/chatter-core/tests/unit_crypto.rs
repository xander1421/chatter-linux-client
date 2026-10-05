//! Unit tests for cryptographic operations.
//!
//! Tests Signal Protocol components, key generation, session management,
//! and encryption/decryption flows using the testing framework.

use crate::framework;

use framework::prelude::*;

// ============================================================================
// Key Generation Tests
// ============================================================================

#[tokio::test]
async fn test_identity_key_pair_generation() {
    let (private, public) = CryptoBuilder::identity_key_pair();

    assert_eq!(
        private.len(),
        32,
        "Private key should be 32 bytes (Curve25519)"
    );
    assert_eq!(
        public.len(),
        33,
        "Public key should be 33 bytes (type prefix + 32 bytes)"
    );
    assert_eq!(
        public[0], 0x05,
        "Public key should have DJB type prefix (0x05)"
    );
}

#[tokio::test]
async fn test_identity_key_uniqueness() {
    let (priv1, pub1) = CryptoBuilder::identity_key_pair();
    let (priv2, pub2) = CryptoBuilder::identity_key_pair();

    assert_ne!(priv1, priv2, "Private keys should be unique");
    assert_ne!(pub1, pub2, "Public keys should be unique");
}

#[tokio::test]
async fn test_prekey_generation() {
    let (id, private, public) = CryptoBuilder::prekey(1);

    assert_eq!(id, 1, "Prekey ID should match");
    assert_eq!(private.len(), 32, "Private key should be 32 bytes");
    assert_eq!(
        public.len(),
        33,
        "Public key should be 33 bytes with type prefix"
    );
    assert_eq!(public[0], 0x05, "Public key should have DJB type prefix");
}

#[tokio::test]
async fn test_signed_prekey_generation() {
    let (id, private, public, signature) = CryptoBuilder::signed_prekey(1);

    assert_eq!(id, 1, "Signed prekey ID should match");
    assert_eq!(private.len(), 32, "Private key should be 32 bytes");
    assert_eq!(public.len(), 33, "Public key should be 33 bytes");
    assert_eq!(signature.len(), 64, "Ed25519 signature should be 64 bytes");
}

#[tokio::test]
async fn test_kyber_prekey_generation() {
    let (id, public, signature) = CryptoBuilder::kyber_prekey(1);

    assert_eq!(id, 1, "Kyber prekey ID should match");
    assert_eq!(
        public.len(),
        1568,
        "Kyber1024 public key should be 1568 bytes"
    );
    assert_eq!(signature.len(), 64, "Signature should be 64 bytes");
}

#[tokio::test]
async fn test_registration_id_range() {
    // Registration IDs should be in valid range (1-16380)
    for _ in 0..100 {
        let reg_id = CryptoBuilder::registration_id();
        assert!(reg_id >= 1, "Registration ID should be at least 1");
        assert!(reg_id <= 16380, "Registration ID should be at most 16380");
    }
}

#[tokio::test]
async fn test_sender_certificate_generation() {
    let cert = CryptoBuilder::sender_certificate();
    assert_eq!(cert.len(), 256, "Sender certificate should be 256 bytes");
}

#[tokio::test]
async fn test_message_key_generation() {
    let key = CryptoBuilder::message_key();
    assert_eq!(key.len(), 32, "Message key should be 32 bytes");
}

#[tokio::test]
async fn test_chain_key_generation() {
    let key = CryptoBuilder::chain_key();
    assert_eq!(key.len(), 32, "Chain key should be 32 bytes");
}

#[tokio::test]
async fn test_sender_key_generation() {
    let key = CryptoBuilder::sender_key();
    assert_eq!(key.len(), 32, "Sender key should be 32 bytes");
}

#[tokio::test]
async fn test_distribution_id_generation() {
    let dist_id = CryptoBuilder::distribution_id();
    assert_eq!(
        dist_id.len(),
        16,
        "Distribution ID (UUID) should be 16 bytes"
    );
}

// ============================================================================
// Prekey Bundle Tests
// ============================================================================

#[tokio::test]
async fn test_prekey_bundle_generation() {
    let (bundle, identity_key) = generate_prekey_bundle();

    assert!(!identity_key.is_empty(), "Identity key should not be empty");
    assert!(
        !bundle.identity_key.is_empty(),
        "Bundle identity key should not be empty"
    );
    assert!(
        bundle.registration_id > 0,
        "Registration ID should be positive"
    );
    assert!(bundle.device_id > 0, "Device ID should be positive");
    assert!(bundle.signed_prekey.is_some(), "Should have signed prekey");
}

#[tokio::test]
async fn test_prekey_bundle_signed_prekey_structure() {
    let (bundle, _) = generate_prekey_bundle();

    let signed = bundle.signed_prekey.expect("Should have signed prekey");
    assert!(signed.id > 0, "Signed prekey ID should be positive");
    assert!(
        !signed.public_key.is_empty(),
        "Signed prekey public key should not be empty"
    );
    assert!(
        !signed.signature.is_empty(),
        "Signed prekey signature should not be empty"
    );
    assert!(
        signed.timestamp > 0,
        "Signed prekey timestamp should be set"
    );
}

// ============================================================================
// In-Memory Signal Store Tests
// ============================================================================

#[tokio::test]
async fn test_signal_store_identity() {
    use framework::mocks::InMemorySignalStores;

    let stores = InMemorySignalStores::new();

    let (private, public) = stores.identity_key_pair();
    assert_eq!(private.len(), 32);
    assert_eq!(public.len(), 33);
    assert_eq!(public[0], 0x05);
}

#[tokio::test]
async fn test_signal_store_registration_id() {
    use framework::mocks::InMemorySignalStores;

    let stores = InMemorySignalStores::new();
    let reg_id = stores.registration_id();

    assert!(
        reg_id >= 1 && reg_id <= 16380,
        "Registration ID in valid range"
    );
}

#[tokio::test]
async fn test_signal_store_session_operations() {
    use framework::mocks::InMemorySignalStores;

    let stores = InMemorySignalStores::new();
    let address = "alice.1"; // user_id.device_id format

    // Initially no session
    assert!(!stores.has_session(address));
    assert!(stores.load_session(address).is_none());

    // Store a session
    let session_data = vec![1, 2, 3, 4, 5];
    stores.store_session(address, session_data.clone());

    // Now session should exist
    assert!(stores.has_session(address));

    let loaded = stores.load_session(address);
    assert_eq!(loaded, Some(session_data));
}

#[tokio::test]
async fn test_signal_store_clear_sessions() {
    use framework::mocks::InMemorySignalStores;

    let stores = InMemorySignalStores::new();

    // Store multiple sessions
    stores.store_session("alice.1", vec![1, 2, 3]);
    stores.store_session("bob.1", vec![4, 5, 6]);

    assert!(stores.has_session("alice.1"));
    assert!(stores.has_session("bob.1"));

    // Clear all sessions
    stores.clear_sessions();

    assert!(!stores.has_session("alice.1"));
    assert!(!stores.has_session("bob.1"));
}

#[tokio::test]
async fn test_signal_store_identity_operations() {
    use framework::mocks::InMemorySignalStores;

    let stores = InMemorySignalStores::new();
    let address = "alice.1";

    // Initially no stored identity
    assert!(stores.load_identity(address).is_none());

    // Store identity
    let identity = vec![0x05, 1, 2, 3, 4];
    stores.store_identity(address, identity.clone());

    // Load it back
    let loaded = stores.load_identity(address);
    assert_eq!(loaded, Some(identity));
}

#[tokio::test]
async fn test_signal_store_deterministic() {
    use framework::mocks::InMemorySignalStores;

    // Create two stores with same seed
    let stores1 = InMemorySignalStores::deterministic(42);
    let stores2 = InMemorySignalStores::deterministic(42);

    // Should have identical keys
    assert_eq!(
        stores1.identity_key_pair(),
        stores2.identity_key_pair(),
        "Deterministic stores should have same identity"
    );
    assert_eq!(
        stores1.registration_id(),
        stores2.registration_id(),
        "Deterministic stores should have same registration ID"
    );

    // Different seed should produce different keys
    let stores3 = InMemorySignalStores::deterministic(123);
    assert_ne!(
        stores1.identity_key_pair(),
        stores3.identity_key_pair(),
        "Different seeds should produce different keys"
    );
}

// ============================================================================
// Session Builder Tests
// ============================================================================

#[tokio::test]
async fn test_session_builder_basic() {
    let session = SessionBuilder::new("alice", 1).build();

    assert_eq!(session.user_id, "alice");
    assert_eq!(session.device_id, 1);
    assert!(
        !session.established,
        "Session should not be established by default"
    );
}

#[tokio::test]
async fn test_session_builder_established() {
    let session = SessionBuilder::new("alice", 1).established().build();

    assert!(session.established, "Session should be established");
    assert!(
        !session.session_data.is_empty(),
        "Established session should have data"
    );
}

#[tokio::test]
async fn test_session_builder_custom_data() {
    let custom_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let session = SessionBuilder::new("alice", 1)
        .with_data(custom_data.clone())
        .build();

    assert!(
        session.established,
        "Session with data should be established"
    );
    assert_eq!(session.session_data, custom_data);
}

#[tokio::test]
async fn test_session_builder_multi_device() {
    let sessions = SessionBuilder::build_multi_device("alice", 3);

    assert_eq!(sessions.len(), 3);
    assert_eq!(sessions[0].device_id, 1);
    assert_eq!(sessions[1].device_id, 2);
    assert_eq!(sessions[2].device_id, 3);

    for session in &sessions {
        assert_eq!(session.user_id, "alice");
        assert!(session.established);
    }
}

// ============================================================================
// Harness Signal Integration Tests
// ============================================================================

#[tokio::test]
async fn test_harness_signal_stores() {
    let harness = TestHarness::unit().await;

    let (alice, bob) = harness.conversation_pair().await;

    // Initially no sessions between users
    assert!(
        !alice.has_session_with(&bob),
        "Should not have session initially"
    );
}

#[tokio::test]
async fn test_harness_deterministic_crypto() {
    let harness = TestHarness::builder()
        .with_mock_network()
        .with_deterministic_crypto(42)
        .build()
        .await;

    let key_pair = harness.signal_stores.identity_key_pair();
    assert!(!key_pair.0.is_empty());
    assert!(!key_pair.1.is_empty());
}

// ============================================================================
// Crypto Builder Ciphertext Tests
// ============================================================================

#[tokio::test]
async fn test_ciphertext_generation() {
    let ciphertext = CryptoBuilder::ciphertext(256);
    assert_eq!(ciphertext.len(), 256);
}

#[tokio::test]
async fn test_ciphertext_randomness() {
    let ct1 = CryptoBuilder::ciphertext(64);
    let ct2 = CryptoBuilder::ciphertext(64);
    assert_ne!(ct1, ct2, "Ciphertexts should be random");
}

// ============================================================================
// Random Bytes Tests
// ============================================================================

#[tokio::test]
async fn test_random_bytes() {
    let bytes1 = random_bytes(32);
    let bytes2 = random_bytes(32);

    assert_eq!(bytes1.len(), 32);
    assert_eq!(bytes2.len(), 32);
    assert_ne!(bytes1, bytes2, "Random bytes should be unique");
}

// ============================================================================
// Attachment Crypto Tests
// ============================================================================

#[tokio::test]
async fn test_attachment_has_encryption_metadata() {
    let attachment = AttachmentBuilder::image("test.jpg").build();

    // Key should be 32 bytes (AES-256)
    assert_eq!(
        attachment.key.len(),
        32,
        "Encryption key should be 32 bytes"
    );

    // Digest should be 32 bytes (SHA-256)
    assert_eq!(attachment.digest.len(), 32, "Digest should be 32 bytes");
}

#[tokio::test]
async fn test_attachment_unique_keys() {
    let att1 = AttachmentBuilder::image("1.jpg").build();
    let att2 = AttachmentBuilder::image("2.jpg").build();

    assert_ne!(att1.key, att2.key, "Each attachment should have unique key");
    assert_ne!(
        att1.digest, att2.digest,
        "Each attachment should have unique digest"
    );
}

// ============================================================================
// Mock Signal Service Trait Tests
// ============================================================================

#[tokio::test]
async fn test_mock_signal_service_trait() {
    use framework::mocks::{MockError, MockResult};

    // Test that MockResult works with crypto operations
    let encrypt_result: MockResult<Vec<u8>> = Ok(CryptoBuilder::ciphertext(128));
    assert!(encrypt_result.is_ok());

    let decrypt_result: MockResult<Vec<u8>> = Err(MockError::CryptoError("Invalid key".into()));
    assert!(decrypt_result.is_err());
}

// ============================================================================
// X3DH Prekey Scenarios
// ============================================================================

#[tokio::test]
async fn test_prekey_one_time_prekey() {
    // Test fixture for one-time prekey
    let (id, private, public) = CryptoBuilder::prekey(1);

    // One-time prekeys are single-use
    assert_eq!(id, 1);
    assert_eq!(private.len(), 32);
    assert_eq!(public.len(), 33);
}

#[tokio::test]
async fn test_multiple_prekeys() {
    // Generate batch of prekeys (like in registration)
    let prekeys: Vec<_> = (1..=100).map(|id| CryptoBuilder::prekey(id)).collect();

    assert_eq!(prekeys.len(), 100);

    // All should have unique public keys
    let public_keys: std::collections::HashSet<_> =
        prekeys.iter().map(|(_, _, pk)| pk.clone()).collect();
    assert_eq!(public_keys.len(), 100, "All prekeys should be unique");
}

// ============================================================================
// Sender Key Tests (Group Messaging)
// ============================================================================

#[tokio::test]
async fn test_sender_key_for_group() {
    let sender_key = CryptoBuilder::sender_key();
    assert_eq!(sender_key.len(), 32);
}

#[tokio::test]
async fn test_distribution_id_uuid_format() {
    let dist_id = CryptoBuilder::distribution_id();

    // Should be valid UUID bytes (16 bytes)
    assert_eq!(dist_id.len(), 16);

    // Can be converted to UUID
    let uuid = uuid::Uuid::from_slice(&dist_id);
    assert!(uuid.is_ok(), "Distribution ID should be valid UUID");
}
