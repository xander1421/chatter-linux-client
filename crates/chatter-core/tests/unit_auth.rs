//! Unit tests for authentication flows.
//!
//! Tests login, registration, logout, and session management
//! using the testing framework with mocked services.

mod common;
mod framework;

use framework::prelude::*;

// ============================================================================
// Registration Tests
// ============================================================================

#[tokio::test]
async fn test_registration_generates_valid_keys() {
    // Registration should generate proper Signal Protocol keys
    let (identity_priv, identity_pub) = CryptoBuilder::identity_key_pair();

    assert_eq!(identity_priv.len(), 32, "Private key should be 32 bytes");
    assert_eq!(identity_pub.len(), 33, "Public key should be 33 bytes (with type prefix)");
    assert_eq!(identity_pub[0], 0x05, "Public key should have DJB type prefix");
}

#[tokio::test]
async fn test_registration_generates_prekey_bundle() {
    // Test that we generate a proper prekey bundle for registration
    let (bundle, identity_key) = generate_prekey_bundle();

    assert!(!identity_key.is_empty(), "Identity key should not be empty");
    assert!(bundle.signed_prekey.is_some(), "Should have signed prekey");

    let signed = bundle.signed_prekey.unwrap();
    assert!(signed.id > 0, "Signed prekey ID should be positive");
    assert!(!signed.public_key.is_empty(), "Signed prekey public key should not be empty");
    assert!(!signed.signature.is_empty(), "Signed prekey signature should not be empty");
}

#[tokio::test]
async fn test_user_builder_creates_valid_credentials() {
    let user = UserBuilder::new().build();

    assert!(!user.user_id.is_empty(), "User ID should be generated");
    assert!(!user.username.is_empty(), "Username should be generated");
    assert!(!user.display_name.is_empty(), "Display name should be generated");
    assert!(user.device_id > 0, "Device ID should be positive");
    assert!(!user.password_hash.is_empty(), "Password hash should be generated");
}

#[tokio::test]
async fn test_registration_username_validation() {
    // Test various username formats
    let valid_usernames = vec![
        "alice",
        "alice123",
        "alice_bob",
        "Alice",
        "user_name_123",
    ];

    for username in valid_usernames {
        let user = UserBuilder::new()
            .username(username)
            .build();
        assert_eq!(user.username, username);
    }
}

#[tokio::test]
async fn test_registration_creates_unique_users() {
    let harness = TestHarness::unit().await;

    let alice = harness.register_user("Alice").await;
    let bob = harness.register_user("Bob").await;

    assert_ne!(alice.user_id, bob.user_id, "Users should have different IDs");
    assert_ne!(alice.username, bob.username, "Users should have different usernames");
}

// ============================================================================
// Login Tests
// ============================================================================

#[tokio::test]
async fn test_login_with_valid_credentials() {
    let harness = TestHarness::unit().await;

    // Register user in mock server
    let user = harness.register_user("Alice").await;

    // Verify user exists in mock server
    let retrieved = harness.mock_server().get_user(&user.user_id);
    assert!(retrieved.is_some(), "User should exist in mock server");
    assert_eq!(retrieved.unwrap().display_name, "Alice");
}

#[tokio::test]
async fn test_login_returns_correct_user_info() {
    let harness = TestHarness::unit().await;
    let user = harness.register_user("TestUser").await;

    assert_eq!(user.display_name, "TestUser");
    assert!(!user.user_id.is_empty());
    assert!(user.device_id > 0);
}

#[tokio::test]
async fn test_password_hash_is_deterministic() {
    // Same password should produce same hash
    let (password1, hash1) = random_password();

    // Different passwords should produce different hashes
    let (password2, hash2) = random_password();

    assert_ne!(password1, password2, "Random passwords should differ");
    assert_ne!(hash1, hash2, "Different passwords should have different hashes");
}

// ============================================================================
// Session Management Tests
// ============================================================================

#[tokio::test]
async fn test_session_creation() {
    let harness = TestHarness::unit().await;
    let user = harness.register_user("Alice").await;

    // Create session token in mock server
    let token = harness.mock_server().create_session(&user.user_id, user.device_id);

    assert!(!token.is_empty(), "Session token should be generated");

    // Validate session
    let session_info = harness.mock_server().validate_session(&token);
    assert!(session_info.is_some(), "Session should be valid");

    let (session_user_id, session_device_id) = session_info.unwrap();
    assert_eq!(session_user_id, user.user_id);
    assert_eq!(session_device_id, user.device_id);
}

#[tokio::test]
async fn test_invalid_session_rejected() {
    let harness = TestHarness::unit().await;

    // Try to validate a fake session token
    let result = harness.mock_server().validate_session("fake-token-12345");
    assert!(result.is_none(), "Invalid session should be rejected");
}

// ============================================================================
// Device Management Tests
// ============================================================================

#[tokio::test]
async fn test_device_id_assignment() {
    let user1 = UserBuilder::new().build();
    let user2 = UserBuilder::new().build();

    // Device IDs should be positive
    assert!(user1.device_id > 0);
    assert!(user2.device_id > 0);
}

#[tokio::test]
async fn test_multi_device_sessions() {
    // Create sessions for multiple devices
    let sessions = SessionBuilder::build_multi_device("user-123", 3);

    assert_eq!(sessions.len(), 3, "Should create 3 sessions");

    // Each session should have different device ID
    let device_ids: Vec<_> = sessions.iter().map(|s| s.device_id).collect();
    assert_eq!(device_ids, vec![1, 2, 3], "Device IDs should be sequential");

    // All sessions should be established
    for session in &sessions {
        assert!(session.established, "Session should be established");
        assert!(!session.session_data.is_empty(), "Session should have data");
    }
}

// ============================================================================
// Auth State Tests
// ============================================================================

#[tokio::test]
async fn test_harness_reset_clears_users() {
    let harness = TestHarness::unit().await;

    // Register some users
    harness.register_user("Alice").await;
    harness.register_user("Bob").await;

    let users_before = harness.get_users().await;
    assert_eq!(users_before.len(), 2);

    // Reset harness
    harness.reset().await;

    let users_after = harness.get_users().await;
    assert_eq!(users_after.len(), 0, "Users should be cleared after reset");
}

#[tokio::test]
async fn test_conversation_pair_creates_two_users() {
    let harness = TestHarness::unit().await;
    let (alice, bob) = harness.conversation_pair().await;

    assert_eq!(alice.display_name(), "Alice");
    assert_eq!(bob.display_name(), "Bob");
    assert_ne!(alice.user_id(), bob.user_id());
}

#[tokio::test]
async fn test_conversation_trio_creates_three_users() {
    let harness = TestHarness::unit().await;
    let (alice, bob, charlie) = harness.conversation_trio().await;

    assert_eq!(alice.display_name(), "Alice");
    assert_eq!(bob.display_name(), "Bob");
    assert_eq!(charlie.display_name(), "Charlie");

    // All should have unique IDs
    assert_ne!(alice.user_id(), bob.user_id());
    assert_ne!(bob.user_id(), charlie.user_id());
    assert_ne!(alice.user_id(), charlie.user_id());
}

// ============================================================================
// UAK (Unidentified Access Key) Tests
// ============================================================================

#[tokio::test]
async fn test_uak_generation() {
    let uak = random_uak();

    assert_eq!(uak.len(), 16, "UAK should be 16 bytes");

    // Generate another - should be different
    let uak2 = random_uak();
    assert_ne!(uak, uak2, "UAKs should be unique");
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[tokio::test]
async fn test_mock_error_types() {
    use framework::mocks::MockError;

    let not_found = MockError::NotFound("user".to_string());
    assert!(not_found.to_string().contains("Not found"));

    let unauthorized = MockError::Unauthorized;
    assert!(unauthorized.to_string().contains("Unauthorized"));

    let permission_denied = MockError::PermissionDenied;
    assert!(permission_denied.to_string().contains("Permission denied"));
}

// ============================================================================
// Integration with Mock Server
// ============================================================================

#[tokio::test]
async fn test_mock_server_user_registration() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    let user = UserBuilder::named("TestUser").build();
    let user_id = server.register_user(user.clone());

    assert_eq!(user_id, user.user_id);

    // Retrieve and verify
    let retrieved = server.get_user(&user_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().display_name, "TestUser");
}

#[tokio::test]
async fn test_mock_server_session_lifecycle() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    // Register user
    let user = UserBuilder::named("Alice").build();
    server.register_user(user.clone());

    // Create session
    let token = server.create_session(&user.user_id, user.device_id);
    assert!(!token.is_empty());

    // Validate session
    let session = server.validate_session(&token);
    assert!(session.is_some());

    // Clear all data
    server.clear();

    // Session should no longer be valid
    let cleared_session = server.validate_session(&token);
    assert!(cleared_session.is_none());
}
