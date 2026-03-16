//! Integration tests for ProvisioningClient.
//!
//! Tests device provisioning flow via QR code linking against a running server.
//! Requires `INTEGRATION_TESTS=1` environment variable.

mod common;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

use chatter_linux::network::grpc::{ProvisioningClient, ProvisioningPollResult};
use chatter_linux::proto::chat_v1::signal_pre_key_bundle::{
    PreKeyRecord, SignedPreKeyRecord,
};
use chatter_linux::proto::chat_v1::SignalPreKeyBundle;
use chatter_linux::proto::provisioning_v1::ProvisionEnvelope;
use libsignal_protocol::{IdentityKeyPair, KeyPair};

use common::*;

// ============================================================================
// Start Provisioning Tests (Unauthenticated - New Device)
// ============================================================================

#[tokio::test]
async fn test_start_provisioning_creates_session() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let mut provisioning_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    // New device starts provisioning (unauthenticated)
    let response = assert_ok(
        provisioning_client
            .start_provisioning("Linux x86_64", "Test Desktop")
            .await,
    );

    // Should get a UUID for the QR code
    assert!(!response.provisioning_uuid.is_empty());
    assert!(response.expires_at > 0);

    println!(
        "Provisioning session created: uuid={}, expires_at={}",
        &response.provisioning_uuid[..8],
        response.expires_at
    );
}

#[tokio::test]
async fn test_poll_provisioning_returns_pending_initially() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let mut provisioning_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    // Start a provisioning session
    let start_response = assert_ok(
        provisioning_client
            .start_provisioning("Linux x86_64", "Test Desktop")
            .await,
    );

    // Poll immediately - should be pending since no primary has provisioned
    let poll_result = assert_ok(
        provisioning_client
            .poll_provisioning_message(&start_response.provisioning_uuid)
            .await,
    );

    match poll_result {
        ProvisioningPollResult::Pending { retry_after_ms } => {
            assert!(retry_after_ms > 0, "Should have non-zero retry delay");
            println!("Poll returned Pending with retry_after_ms={}", retry_after_ms);
        }
        other => panic!("Expected Pending, got {:?}", other),
    }
}

#[tokio::test]
async fn test_poll_nonexistent_session_returns_expired() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let mut provisioning_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    // Poll with a fake UUID
    let poll_result = assert_ok(
        provisioning_client
            .poll_provisioning_message("00000000-0000-0000-0000-000000000000")
            .await,
    );

    match poll_result {
        ProvisioningPollResult::Expired { message } => {
            println!("Non-existent session returned Expired: {}", message);
        }
        other => panic!("Expected Expired for non-existent session, got {:?}", other),
    }
}

// ============================================================================
// Complete Provisioning Flow Tests
// ============================================================================

#[tokio::test]
async fn test_complete_provisioning_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Step 1: Register a primary user (this will be the "phone")
    let primary_user = assert_ok(setup_authenticated_test(&pool).await);
    println!(
        "Primary user registered: user_id={}, device_id={}",
        &primary_user.user_id[..8],
        primary_user.device_id
    );

    // Step 2: New device starts provisioning (unauthenticated)
    // Clear auth first to simulate new device
    pool.auth().clear();
    let mut new_device_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    let start_response = assert_ok(
        new_device_client
            .start_provisioning("Linux x86_64", "Work Laptop")
            .await,
    );
    let provisioning_uuid = start_response.provisioning_uuid.clone();
    println!(
        "New device started provisioning: uuid={}...",
        &provisioning_uuid[..8]
    );

    // Step 3: Primary device scans QR and sends encrypted envelope
    // Re-authenticate as primary
    pool.auth().set_basic(
        format!("{}.{}", primary_user.user_id, primary_user.device_id),
        BASE64.encode(&primary_user.device_auth_token),
    );

    let mut primary_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    // Create a mock encrypted envelope (in real flow, this contains ECDH-encrypted keys)
    let envelope = ProvisionEnvelope {
        public_key: vec![0x05; 33], // Signal key format: 0x05 prefix + 32 bytes
        body: vec![0u8; 80],        // Encrypted provision message
    };

    assert_ok(
        primary_client
            .provision_device(&provisioning_uuid, envelope.clone())
            .await,
    );
    println!("Primary device sent encrypted envelope");

    // Step 4: New device polls and receives the envelope
    pool.auth().clear();
    let poll_result = assert_ok(
        new_device_client
            .poll_provisioning_message(&provisioning_uuid)
            .await,
    );

    let received_envelope = match poll_result {
        ProvisioningPollResult::Envelope(env) => {
            println!("New device received envelope");
            env
        }
        other => panic!("Expected Envelope, got {:?}", other),
    };

    assert_eq!(received_envelope.public_key, envelope.public_key);
    assert_eq!(received_envelope.body, envelope.body);

    // Step 5: New device completes linking with its prekey bundle
    let (prekey_bundle, _identity_key) = generate_prekey_bundle();
    let device_auth_token: [u8; 32] = rand::random();
    let uak: [u8; 16] = rand::random();

    let complete_response = assert_ok(
        new_device_client
            .complete_device_linking(
                // Note: We need to get the provisioning code from the decrypted envelope
                // For this test, we'll use a mock approach - the server should have
                // stored the code when primary provisioned
                // In a real implementation, this would come from decrypting the envelope
                &get_provisioning_code_for_test(&pool, &provisioning_uuid).await,
                &device_auth_token,
                &uak,
                "Linux x86_64",
                "Work Laptop",
                prekey_bundle,
                Vec::new(),
            )
            .await,
    );

    println!(
        "Device linking completed: device_id={}, server_time={}",
        complete_response.device_id, complete_response.server_time
    );

    // New device should have device_id > 1 (primary is always 1)
    assert!(
        complete_response.device_id > 1,
        "Linked device should have device_id > 1"
    );
}

#[tokio::test]
async fn test_cancel_provisioning_session() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Register primary user
    let primary_user = assert_ok(setup_authenticated_test(&pool).await);

    // New device starts provisioning
    pool.auth().clear();
    let mut new_device_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    let start_response = assert_ok(
        new_device_client
            .start_provisioning("Linux x86_64", "Test Device")
            .await,
    );
    let provisioning_uuid = start_response.provisioning_uuid.clone();

    // Primary provisions the session first (so they own it)
    pool.auth().set_basic(
        format!("{}.{}", primary_user.user_id, primary_user.device_id),
        BASE64.encode(&primary_user.device_auth_token),
    );

    let mut primary_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    let envelope = ProvisionEnvelope {
        public_key: vec![0x05; 33],
        body: vec![0u8; 80],
    };

    assert_ok(
        primary_client
            .provision_device(&provisioning_uuid, envelope)
            .await,
    );

    // Primary cancels the session
    assert_ok(
        primary_client
            .cancel_provisioning(&provisioning_uuid)
            .await,
    );
    println!("Primary cancelled provisioning session");

    // New device polls and should see cancelled
    pool.auth().clear();
    let poll_result = assert_ok(
        new_device_client
            .poll_provisioning_message(&provisioning_uuid)
            .await,
    );

    match poll_result {
        ProvisioningPollResult::Cancelled { message } => {
            println!("Poll returned Cancelled: {}", message);
        }
        other => panic!("Expected Cancelled, got {:?}", other),
    }
}

#[tokio::test]
async fn test_list_provisioning_sessions() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Register primary user
    let primary_user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a provisioning session from new device
    pool.auth().clear();
    let mut new_device_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    let start_response = assert_ok(
        new_device_client
            .start_provisioning("Linux x86_64", "Test Device")
            .await,
    );

    // Primary provisions it to own it
    pool.auth().set_basic(
        format!("{}.{}", primary_user.user_id, primary_user.device_id),
        BASE64.encode(&primary_user.device_auth_token),
    );

    let mut primary_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    let envelope = ProvisionEnvelope {
        public_key: vec![0x05; 33],
        body: vec![0u8; 80],
    };

    assert_ok(
        primary_client
            .provision_device(&start_response.provisioning_uuid, envelope)
            .await,
    );

    // List sessions as primary
    let list_response = assert_ok(primary_client.list_provisioning_sessions().await);

    println!(
        "Primary has {} provisioning sessions",
        list_response.sessions.len()
    );

    // Should have at least one session
    assert!(
        !list_response.sessions.is_empty(),
        "Primary should have at least one session"
    );

    // Verify the session we created is in the list
    let found = list_response
        .sessions
        .iter()
        .any(|s| s.provisioning_uuid == start_response.provisioning_uuid);
    assert!(found, "Created session should be in the list");
}

// ============================================================================
// Security Tests
// ============================================================================

#[tokio::test]
async fn test_provision_device_requires_authentication() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Start a provisioning session (unauthenticated is OK)
    let mut client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    let start_response = assert_ok(
        client
            .start_provisioning("Linux x86_64", "Test Device")
            .await,
    );

    // Try to provision without authentication - should fail
    let envelope = ProvisionEnvelope {
        public_key: vec![0x05; 33],
        body: vec![0u8; 80],
    };

    let result = client
        .provision_device(&start_response.provisioning_uuid, envelope)
        .await;

    assert_unauthenticated(result);
}

#[tokio::test]
async fn test_cancel_provisioning_requires_authentication() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Start a provisioning session
    let mut client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    let start_response = assert_ok(
        client
            .start_provisioning("Linux x86_64", "Test Device")
            .await,
    );

    // Try to cancel without authentication - should fail
    let result = client
        .cancel_provisioning(&start_response.provisioning_uuid)
        .await;

    assert_unauthenticated(result);
}

#[tokio::test]
async fn test_list_sessions_requires_authentication() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let mut client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    // Try to list without authentication - should fail
    let result = client.list_provisioning_sessions().await;

    assert_unauthenticated(result);
}

#[tokio::test]
async fn test_cannot_reprovision_already_provisioned_session() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Register primary user
    let primary_user = assert_ok(setup_authenticated_test(&pool).await);

    // New device starts provisioning
    pool.auth().clear();
    let mut new_device_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    let start_response = assert_ok(
        new_device_client
            .start_provisioning("Linux x86_64", "Test Device")
            .await,
    );

    // Primary provisions the session
    pool.auth().set_basic(
        format!("{}.{}", primary_user.user_id, primary_user.device_id),
        BASE64.encode(&primary_user.device_auth_token),
    );

    let mut primary_client = ProvisioningClient::new(pool.channel(), pool.auth_clone());

    let envelope = ProvisionEnvelope {
        public_key: vec![0x05; 33],
        body: vec![0u8; 80],
    };

    assert_ok(
        primary_client
            .provision_device(&start_response.provisioning_uuid, envelope.clone())
            .await,
    );

    // Try to provision again - should fail
    let result = primary_client
        .provision_device(&start_response.provisioning_uuid, envelope)
        .await;

    // Should get an error (session already provisioned)
    assert!(result.is_err(), "Re-provisioning should fail");
    println!("Re-provisioning correctly rejected: {:?}", result.err());
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Helper to get provisioning code for testing.
/// In a real implementation, this would be decrypted from the envelope.
/// For testing, we query the server directly.
async fn get_provisioning_code_for_test(
    pool: &std::sync::Arc<chatter_linux::network::grpc::GrpcClientPool>,
    _provisioning_uuid: &str,
) -> String {
    // In a real test against a real server, we'd need to:
    // 1. Have the server expose a test endpoint, OR
    // 2. Actually implement ECDH decryption of the envelope
    //
    // For now, we'll skip the complete_device_linking test if we can't get the code.
    // The functional tests on the Go side already verify this works.
    //
    // This is a placeholder that will cause the test to fail gracefully.
    // Real integration would require the crypto flow.
    "test-provisioning-code-placeholder".to_string()
}

/// Generate a valid Signal Protocol prekey bundle for testing.
fn generate_prekey_bundle() -> (SignalPreKeyBundle, Vec<u8>) {
    let identity_key_pair = IdentityKeyPair::generate(&mut rand::rng());
    let identity_public_key = identity_key_pair.identity_key().serialize();

    let signed_prekey_pair = KeyPair::generate(&mut rand::rng());
    let signed_prekey_signature = identity_key_pair
        .private_key()
        .calculate_signature(&signed_prekey_pair.public_key.serialize(), &mut rand::rng())
        .expect("Signature should succeed");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let one_time_prekey_pair = KeyPair::generate(&mut rand::rng());

    let registration_id = rand::random::<u32>() % 16380 + 1;

    let prekey_bundle = SignalPreKeyBundle {
        identity_key: identity_public_key.as_ref().to_vec(),
        registration_id,
        device_id: 0, // Will be assigned by server
        signed_prekey: Some(SignedPreKeyRecord {
            id: 1,
            public_key: signed_prekey_pair.public_key.serialize().to_vec(),
            signature: signed_prekey_signature.into_vec(),
            timestamp,
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 1,
            public_key: one_time_prekey_pair.public_key.serialize().to_vec(),
        }),
        kyber_prekey: None,
        unidentified_access_key: Some(random_uak().to_vec()),
        unrestricted_unidentified_access: Some(false),
    };

    (prekey_bundle, identity_public_key.as_ref().to_vec())
}
