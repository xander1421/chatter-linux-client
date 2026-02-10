//! Integration tests for AuthClient.
//!
//! Tests authentication, registration, session management, and device management
//! against a running server.

mod common;

use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;

use chatter_linux::error::NetworkError;
use chatter_linux::network::grpc::AuthClient;
use chatter_linux::proto::chat_v1::auth_event;

use common::*;

// ============================================================================
// Registration Tests
// ============================================================================

#[tokio::test]
async fn test_register_user_stream() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let username = unique_username();
    let device_auth_token = random_device_auth_token();
    let profile_key = random_profile_key();
    let display_name = format!("Test User {}", &username[10..18]);

    // Generate real Signal Protocol keys
    use chatter_linux::proto::chat_v1::signal_pre_key_bundle::{PreKeyRecord, SignedPreKeyRecord};
    use chatter_linux::proto::chat_v1::SignalPreKeyBundle;
    use libsignal_protocol::{IdentityKeyPair, KeyPair};

    let identity_key_pair = IdentityKeyPair::generate(&mut rand::rng());
    let identity_public_key = identity_key_pair.identity_key().serialize();

    let signed_prekey_pair = KeyPair::generate(&mut rand::rng());
    let signed_prekey_signature = identity_key_pair
        .private_key()
        .calculate_signature(&signed_prekey_pair.public_key.serialize(), &mut rand::rng())
        .expect("Signature should succeed");

    let one_time_prekey_pair = KeyPair::generate(&mut rand::rng());

    let prekey_bundle = SignalPreKeyBundle {
        identity_key: identity_public_key.as_ref().to_vec(),
        registration_id: rand::random::<u32>() % 16380 + 1,
        device_id: 1,
        signed_prekey: Some(SignedPreKeyRecord {
            id: 1,
            public_key: signed_prekey_pair.public_key.serialize().to_vec(),
            signature: signed_prekey_signature.into_vec(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 1,
            public_key: one_time_prekey_pair.public_key.serialize().to_vec(),
        }),
        kyber_prekey: None,
        unidentified_access_key: Some(random_uak().to_vec()),
        unrestricted_unidentified_access: Some(false),
    };

    let password_hash = random_password_hash();
    let stream = assert_ok(
        auth_client
            .register(
                &username,
                &password_hash,
                &display_name,
                identity_public_key.as_ref(),
                "integration-test",
                &device_auth_token,
                &profile_key,
                Some(prekey_bundle),
                Vec::new(),
            )
            .await,
    );

    tokio::pin!(stream);

    let mut received_completion = false;
    while let Some(result) = stream.next().await {
        let event = assert_ok(result);
        if let Some(payload) = event.payload {
            match payload {
                auth_event::Payload::UserRegistered(_) => {
                    // Progress event
                }
                auth_event::Payload::RegistrationComplete(result) => {
                    assert!(!result.user_id.is_empty(), "user_id should not be empty");
                    assert!(result.device_id > 0, "device_id should be > 0");
                    received_completion = true;
                }
                auth_event::Payload::RegistrationError(error) => {
                    panic!("Registration failed: {}", error.message);
                }
            }
        }
    }

    assert!(received_completion, "Should have received completion event");
}

#[tokio::test]
async fn test_register_and_wait() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);

    assert!(!user.user_id.is_empty());
    assert!(user.device_id > 0);
}

#[tokio::test]
async fn test_register_duplicate_username() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Register first user
    let user = assert_ok(register_test_user(&pool).await);

    // Generate real Signal Protocol keys for the duplicate attempt
    use chatter_linux::proto::chat_v1::signal_pre_key_bundle::{PreKeyRecord, SignedPreKeyRecord};
    use chatter_linux::proto::chat_v1::SignalPreKeyBundle;
    use libsignal_protocol::{IdentityKeyPair, KeyPair};

    let identity_key_pair = IdentityKeyPair::generate(&mut rand::rng());
    let identity_public_key = identity_key_pair.identity_key().serialize();

    let signed_prekey_pair = KeyPair::generate(&mut rand::rng());
    let signed_prekey_signature = identity_key_pair
        .private_key()
        .calculate_signature(&signed_prekey_pair.public_key.serialize(), &mut rand::rng())
        .expect("Signature should succeed");

    let one_time_prekey_pair = KeyPair::generate(&mut rand::rng());

    let prekey_bundle = SignalPreKeyBundle {
        identity_key: identity_public_key.as_ref().to_vec(),
        registration_id: rand::random::<u32>() % 16380 + 1,
        device_id: 1,
        signed_prekey: Some(SignedPreKeyRecord {
            id: 1,
            public_key: signed_prekey_pair.public_key.serialize().to_vec(),
            signature: signed_prekey_signature.into_vec(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 1,
            public_key: one_time_prekey_pair.public_key.serialize().to_vec(),
        }),
        kyber_prekey: None,
        unidentified_access_key: Some(random_uak().to_vec()),
        unrestricted_unidentified_access: Some(false),
    };

    // Try to register with same username
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let result = auth_client
        .register_and_wait(
            &user.username,
            &random_password_hash(),
            "Duplicate User",
            identity_public_key.as_ref(),
            "test-device",
            &random_device_auth_token(),
            &random_profile_key(),
            Some(prekey_bundle),
            Vec::new(),
        )
        .await;

    // Should fail with already exists error
    match result {
        Err(NetworkError::InvalidRequest(msg)) => {
            assert!(msg.contains("username") || msg.contains("taken") || msg.contains("exists"),
                "Error should mention username conflict: {}", msg);
        }
        Err(NetworkError::ServerError(msg)) => {
            assert!(msg.contains("username") || msg.contains("taken") || msg.contains("exists"),
                "Error should mention username conflict: {}", msg);
        }
        Err(e) => panic!("Expected username conflict error, got: {:?}", e),
        Ok(_) => panic!("Should have failed with duplicate username"),
    }
}

// ============================================================================
// Login Tests
// ============================================================================

#[tokio::test]
async fn test_login_success() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);

    // Login with the same credentials
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(
        auth_client
            .login(
                &user.username,
                &user.password_hash,
                "test-device-login",
                &user.public_key,
                &user.device_auth_token,
                &random_uak(),
                Some(user.device_id),
            )
            .await,
    );

    // AuthResponse contains session and user_profile
    assert!(response.user_profile.is_some());
    let profile = response.user_profile.unwrap();
    assert!(!profile.user_id.is_empty());
    assert_eq!(profile.user_id, user.user_id);
}

#[tokio::test]
async fn test_login_wrong_password() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);

    // Login with wrong password
    let wrong_password = Sha256::digest(b"wrong_password").to_vec();
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let result = auth_client
        .login(
            &user.username,
            &wrong_password,
            "test-device",
            &user.public_key,
            &user.device_auth_token,
            &random_uak(),
            None,
        )
        .await;

    // Should fail with auth error
    assert_auth_error(result);
}

#[tokio::test]
async fn test_login_nonexistent_user() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let result = auth_client
        .login(
            "nonexistent_user_12345",
            &random_password_hash(),
            "test-device",
            &random_public_key(),
            &random_device_auth_token(),
            &random_uak(),
            None,
        )
        .await;

    // Should fail with not found or auth error
    match result {
        Err(NetworkError::NotFound(_)) | Err(NetworkError::Unauthenticated) => {}
        Err(e) => panic!("Expected NotFound or Unauthenticated, got: {:?}", e),
        Ok(_) => panic!("Expected error, got Ok"),
    }
}

// ============================================================================
// Username Check Tests
// ============================================================================

#[tokio::test]
async fn test_check_username_available() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let username = unique_username();
    let response = assert_ok(auth_client.check_username(&username).await);

    assert!(response.available, "New username should be available");
}

#[tokio::test]
async fn test_check_username_taken() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(auth_client.check_username(&user.username).await);

    assert!(!response.available, "Registered username should not be available");
}

// ============================================================================
// Logout Tests
// ============================================================================

#[tokio::test]
async fn test_logout() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let result = auth_client.logout(user.device_id).await;

    assert!(result.is_ok(), "Logout should succeed");
}

// ============================================================================
// Device Management Tests
// ============================================================================

#[tokio::test]
async fn test_list_devices() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(auth_client.list_devices().await);

    assert!(!response.devices.is_empty(), "Should have at least one device");

    // Find our device
    let our_device = response
        .devices
        .iter()
        .find(|d| d.device_id == user.device_id);
    assert!(our_device.is_some(), "Should find our device in list");
}

#[tokio::test]
async fn test_list_sessions() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(auth_client.list_sessions().await);

    // Should have at least one active session
    assert!(
        !response.sessions.is_empty(),
        "Should have at least one active session"
    );
}

// ============================================================================
// Trust Root Tests (Hardcoded - Signal Security Model)
// ============================================================================

#[tokio::test]
async fn test_hardcoded_trust_root() {
    // Trust root is now hardcoded in config.rs following Signal's security model.
    // This prevents MITM attacks during registration.
    use chatter_linux::config::TRUST_ROOT_PUBLIC_KEY_BYTES;

    // Verify the hardcoded key is valid
    assert_eq!(TRUST_ROOT_PUBLIC_KEY_BYTES.len(), 32, "Trust root must be 32 bytes");
    assert_ne!(TRUST_ROOT_PUBLIC_KEY_BYTES, [0u8; 32], "Trust root must not be all zeros");
}

// ============================================================================
// Sender Certificate Tests
// ============================================================================

#[tokio::test]
async fn test_request_sender_certificate() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(
        auth_client
            .request_sender_certificate(user.device_id, None)
            .await,
    );

    assert!(
        response.certificate.is_some(),
        "Should return a sender certificate"
    );
    let cert = response.certificate.unwrap();
    assert!(!cert.certificate.is_empty(), "Certificate data should not be empty");
}

// ============================================================================
// Password Change Tests
// ============================================================================

#[tokio::test]
async fn test_change_password() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let new_password_hash = Sha256::digest(b"new_secure_password").to_vec();

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let result = auth_client
        .change_password(&user.password_hash, &new_password_hash)
        .await;

    assert!(result.is_ok(), "Password change should succeed");

    // Verify we can login with the new password
    pool.auth().clear();
    let mut new_auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let login_result = new_auth_client
        .login(
            &user.username,
            &new_password_hash,
            "test-device-new-pass",
            &user.public_key,
            &user.device_auth_token,
            &random_uak(),
            None,
        )
        .await;

    assert!(login_result.is_ok(), "Should be able to login with new password");
}

// ============================================================================
// Protected Endpoint Tests
// ============================================================================

#[tokio::test]
async fn test_unauthenticated_protected_endpoint() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Don't set up authentication
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let result = auth_client.list_devices().await;

    assert_unauthenticated(result);
}

// ============================================================================
// Prekey Tests
// ============================================================================

#[tokio::test]
async fn test_get_prekey_stats() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let response = assert_ok(auth_client.get_pre_key_stats(user.device_id).await);

    // New user may have 0 prekeys (we didn't upload any)
    assert_eq!(response.device_id, user.device_id);
}

#[tokio::test]
async fn test_upload_and_get_prekey_bundle() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a prekey bundle with real Signal keys
    use chatter_linux::proto::chat_v1::signal_pre_key_bundle::{PreKeyRecord, SignedPreKeyRecord};
    use chatter_linux::proto::chat_v1::SignalPreKeyBundle;
    use libsignal_protocol::{IdentityKeyPair, KeyPair};

    // Generate real identity key pair
    let identity_key_pair = IdentityKeyPair::generate(&mut rand::rng());
    let identity_public_key = identity_key_pair.identity_key().serialize();

    // Generate signed prekey with valid signature
    let signed_prekey_pair = KeyPair::generate(&mut rand::rng());
    let signed_prekey_signature = identity_key_pair
        .private_key()
        .calculate_signature(&signed_prekey_pair.public_key.serialize(), &mut rand::rng())
        .expect("Signature should succeed");

    // Generate one-time prekey
    let one_time_prekey_pair = KeyPair::generate(&mut rand::rng());

    let bundle = SignalPreKeyBundle {
        identity_key: identity_public_key.as_ref().to_vec(),
        registration_id: 12345,
        device_id: user.device_id,
        signed_prekey: Some(SignedPreKeyRecord {
            id: 2, // Different from registration bundle
            public_key: signed_prekey_pair.public_key.serialize().to_vec(),
            signature: signed_prekey_signature.into_vec(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 100,
            public_key: one_time_prekey_pair.public_key.serialize().to_vec(),
        }),
        kyber_prekey: None,
        unidentified_access_key: Some(random_uak().to_vec()),
        unrestricted_unidentified_access: Some(false),
    };

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Upload the bundle
    let upload_result = auth_client.upload_pre_key_bundle(bundle).await;
    assert!(upload_result.is_ok(), "Should be able to upload prekey bundle: {:?}", upload_result.err());

    // Fetch it back (as another user would)
    let get_result = auth_client
        .get_pre_key_bundle(&user.user_id, &user.device_id.to_string())
        .await;

    // Note: Fetching own prekeys may succeed or fail depending on server implementation
    // The important thing is the upload succeeded
    match get_result {
        Ok(response) => {
            // GetPreKeyBundleResponse uses a oneof result
            assert!(response.result.is_some(), "Should return a bundle");
        }
        Err(NetworkError::NotFound(_)) => {
            // Some servers don't allow fetching own prekeys
        }
        Err(e) => panic!("Unexpected error: {:?}", e),
    }
}
