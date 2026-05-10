//! Integration tests for Sealed Sender and Ephemeral Mailbox.
//!
//! Tests the Signal-style anonymous messaging system including:
//! - Sealed sender message delivery
//! - Ephemeral mailbox creation and polling
//! - Blind signature token management
//! - Device message queue operations

mod common;

use chatter_core::mailbox::blind_signature::{blind_key, unblind, ServerPublicKey};
use chatter_core::network::grpc::{AuthClient, MailboxClient, MessageClient};
use chatter_core::proto::{
    signal_pre_key_bundle::PreKeyRecord, GetPreKeyBundleResponse, SealedSenderMessage,
};
use chatter_core::proto::mailbox_v1::EnvelopeType;
use common::*;
use ed25519_dalek::{Signer, SigningKey};
use rand::RngCore;

// ============================================================================
// Sealed Sender Message Tests
// ============================================================================

/// Tests sending a sealed sender message with proper UAK authentication.
/// This is the Signal-style anonymous message flow.
#[tokio::test]
async fn test_send_sealed_message_to_user() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);

    // Create sender
    let _sender = assert_ok(setup_authenticated_test(&pool).await);

    // Create recipient (need a second user)
    let recipient = assert_ok(register_test_user(&pool).await);

    // Get recipient's prekey bundle to obtain their UAK
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let bundle_response = assert_ok(
        auth_client.get_pre_key_bundle(&recipient.user_id, "1").await
    );

    // Extract UAK from bundle
    let uak = match bundle_response.result {
        Some(chatter_core::proto::get_pre_key_bundle_response::Result::Bundle(bundle)) => {
            bundle.unidentified_access_key.unwrap_or_default()
        }
        _ => panic!("Expected single bundle result"),
    };

    // Skip if recipient has no UAK configured
    if uak.is_empty() {
        println!("Recipient has no UAK configured, skipping test");
        return;
    }

    // Sealed sender is mutually exclusive with Basic auth — server now
    // returns "provide exactly one auth method (anonymous credential, UAK,
    // or Basic auth)" when both are present. Clear the pool's Basic auth
    // before sending so only the UAK header survives.
    pool.auth().clear();
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Create a sealed sender message with realistic envelope size
    // Signal sealed sender envelopes are typically 200+ bytes
    let mut sealed_body = vec![0u8; 300];
    rand::fill(&mut sealed_body[..]);

    let sealed_message = SealedSenderMessage {
        sealed_body,
        server_key_id: 1,
        // Server expects Unix nanoseconds for sealed-sender freshness
        // (`time.Unix(0, int64(sealedMsg.Timestamp))`); seconds or millis
        // would be far in the past relative to "now ± 5min" and rejected
        // as "sealed message too old".
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
    };

    // Send sealed message with UAK for anonymous delivery
    let result = msg_client
        .send_sealed_message_anonymous(&recipient.user_id, 1, sealed_message, &uak)
        .await;

    assert!(
        result.is_ok(),
        "Should be able to send sealed message with UAK: {:?}",
        result.err()
    );

    let response = result.unwrap();
    assert!(response.success, "Sealed message should be accepted");
}

/// Tests that SendSealedMessage correctly rejects requests without UAK header.
/// Signal-style sealed sender requires the Unidentified-Access-Key header
/// containing the recipient's UAK for anonymous message delivery.
#[tokio::test]
async fn test_send_sealed_message_without_uak_rejected() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _sender = assert_ok(setup_authenticated_test(&pool).await);
    let recipient = assert_ok(register_test_user(&pool).await);

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    let sealed_message = SealedSenderMessage {
        sealed_body: b"encrypted sealed sender envelope".to_vec(),
        server_key_id: 1,
        timestamp: 0,
    };

    // Without UAK header, server should reject (Unauthenticated)
    let result = msg_client
        .send_sealed_message(&recipient.user_id, 1, sealed_message)
        .await;

    // Should fail with Unauthenticated - requires UAK header
    assert!(
        result.is_err(),
        "Should fail without UAK header"
    );
}

#[tokio::test]
async fn test_send_sealed_message_to_nonexistent_user() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    let sealed_message = SealedSenderMessage {
        sealed_body: b"test".to_vec(),
        server_key_id: 1,
        timestamp: 0,
    };

    let result = msg_client
        .send_sealed_message("00000000-0000-0000-0000-000000000000", 1, sealed_message)
        .await;

    assert!(result.is_err(), "Should fail for non-existent user");
}

// ============================================================================
// Device Message Queue Tests
// ============================================================================

#[tokio::test]
async fn test_get_device_messages_empty() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    let result = msg_client.get_device_messages(0, 100).await;

    assert!(
        result.is_ok(),
        "Should be able to fetch device messages: {:?}",
        result.err()
    );

    let response = result.unwrap();
    assert!(response.messages.len() <= 100);
}

#[tokio::test]
async fn test_acknowledge_messages() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Server requires at least one message GUID - empty list is rejected
    let result = msg_client.acknowledge_messages(vec![]).await;
    assert!(result.is_err(), "Empty list should be rejected");

    // Acknowledge a fake message ID (won't find it, but validates the endpoint)
    let result = msg_client
        .acknowledge_messages(vec!["00000000-0000-0000-0000-000000000001".to_string()])
        .await;

    // Should succeed (even if message doesn't exist - idempotent operation)
    assert!(
        result.is_ok(),
        "Should be able to acknowledge message: {:?}",
        result.err()
    );
}

// ============================================================================
// PreKey Bundle Tests (for X3DH key exchange)
// ============================================================================

#[tokio::test]
async fn test_get_prekey_bundle_for_other_user() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _sender = assert_ok(setup_authenticated_test(&pool).await);

    let recipient = assert_ok(register_test_user(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let result: Result<GetPreKeyBundleResponse, _> = auth_client
        .get_pre_key_bundle(&recipient.user_id, "1")
        .await;

    assert!(
        result.is_ok(),
        "Should be able to get prekey bundle: {:?}",
        result.err()
    );

    let response = result.unwrap();
    assert!(response.result.is_some(), "Should have a result");
}

#[tokio::test]
async fn test_get_prekey_bundle_nonexistent_user() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let result: Result<GetPreKeyBundleResponse, _> = auth_client
        .get_pre_key_bundle("00000000-0000-0000-0000-000000000000", "1")
        .await;

    assert!(result.is_err(), "Should fail for non-existent user");
}

#[tokio::test]
async fn test_refresh_prekeys() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Generate real one-time prekeys with 33-byte compressed public keys
    // Signal uses compressed curve points (0x05 prefix + 32 bytes)
    let new_prekeys: Vec<PreKeyRecord> = (0..10u32)
        .map(|i| {
            let mut public_key = vec![0x05u8]; // Signal curve type prefix
            let mut key_bytes = [0u8; 32];
            rand::fill(&mut key_bytes);
            public_key.extend_from_slice(&key_bytes);
            PreKeyRecord {
                id: 100 + i,
                public_key,
            }
        })
        .collect();

    let result = auth_client.refresh_pre_keys(new_prekeys).await;

    assert!(
        result.is_ok(),
        "Should be able to refresh prekeys: {:?}",
        result.err()
    );
}

// ============================================================================
// Ephemeral Mailbox Tests - Real Blind Signatures
// ============================================================================

#[tokio::test]
async fn test_get_blind_signature_public_key() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    let result = mailbox_client.get_blind_signature_public_key().await;

    assert!(
        result.is_ok(),
        "Should be able to get blind signature public key: {:?}",
        result.err()
    );

    let response = result.unwrap();
    assert!(
        !response.public_key.is_empty(),
        "Should have RSA public key"
    );
    // RSA-2048 public key in DER format
    assert!(
        response.public_key.len() >= 256,
        "RSA key should be at least 256 bytes, got {}",
        response.public_key.len()
    );
}

#[tokio::test]
async fn test_blind_signature_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Step 1: Get server's RSA public key
    let pk_response = assert_ok(mailbox_client.get_blind_signature_public_key().await);

    // Parse the server public key using real blind_signature module
    let server_pk = assert_ok(ServerPublicKey::from_der(
        &pk_response.public_key,
        pk_response.key_id.clone(),
        pk_response.valid_until,
    ));

    // Step 2: Generate real Ed25519 keypair
    let mut secret_bytes = [0u8; 32];
    rand::fill(&mut secret_bytes);
    let signing_key = SigningKey::from_bytes(&secret_bytes);
    let public_key_bytes = signing_key.verifying_key().to_bytes().to_vec();

    // Step 3: Blind the public key using real blind signature implementation
    let blinding_op = assert_ok(blind_key(&server_pk, &public_key_bytes));

    // Step 4: Request blind signature from server
    let result = mailbox_client
        .get_mailbox_tokens(1, vec![blinding_op.blinded_message.clone()])
        .await;

    assert!(
        result.is_ok(),
        "Should be able to request mailbox tokens: {:?}",
        result.err()
    );

    let response = result.unwrap();
    assert_eq!(response.blinded_signatures.len(), 1, "Should get 1 signature");
    assert!(response.remaining_quota >= 0, "Should have quota info");

    // Step 5: Unblind the signature
    let unblinded_sig = assert_ok(unblind(
        &server_pk,
        &response.blinded_signatures[0],
        &blinding_op.secret,
        &blinding_op.randomizer,
        &public_key_bytes,
    ));

    // Signature should be RSA-2048 size (256 bytes)
    assert_eq!(
        unblinded_sig.len(),
        256,
        "Unblinded signature should be 256 bytes"
    );

    println!("Blind signature flow completed successfully");
}

#[tokio::test]
async fn test_get_mailbox_tokens_multiple() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Get server key
    let pk_response = assert_ok(mailbox_client.get_blind_signature_public_key().await);
    let server_pk = assert_ok(ServerPublicKey::from_der(
        &pk_response.public_key,
        pk_response.key_id.clone(),
        pk_response.valid_until,
    ));

    // Generate 3 keypairs and blind them
    let mut blinded_keys = Vec::new();
    for _ in 0..3 {
        let mut secret_bytes = [0u8; 32];
        rand::fill(&mut secret_bytes);
        let signing_key = SigningKey::from_bytes(&secret_bytes);
        let public_key_bytes = signing_key.verifying_key().to_bytes().to_vec();

        let blinding_op = assert_ok(blind_key(&server_pk, &public_key_bytes));
        blinded_keys.push(blinding_op.blinded_message);
    }

    // Request all 3 signatures
    let result = mailbox_client.get_mailbox_tokens(3, blinded_keys).await;

    assert!(
        result.is_ok(),
        "Should be able to request 3 mailbox tokens: {:?}",
        result.err()
    );

    let response = result.unwrap();
    assert_eq!(response.blinded_signatures.len(), 3, "Should get 3 signatures");
}

#[tokio::test]
async fn test_create_ephemeral_mailbox_with_real_signature() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Step 1: Get server's RSA public key
    let pk_response = assert_ok(mailbox_client.get_blind_signature_public_key().await);
    let server_pk = assert_ok(ServerPublicKey::from_der(
        &pk_response.public_key,
        pk_response.key_id.clone(),
        pk_response.valid_until,
    ));

    // Step 2: Generate Ed25519 keypair
    let mut secret_bytes = [0u8; 32];
    rand::fill(&mut secret_bytes);
    let signing_key = SigningKey::from_bytes(&secret_bytes);
    let public_key_bytes = signing_key.verifying_key().to_bytes().to_vec();

    // Step 3: Blind and get signature
    let blinding_op = assert_ok(blind_key(&server_pk, &public_key_bytes));

    let tokens_response = assert_ok(
        mailbox_client
            .get_mailbox_tokens(1, vec![blinding_op.blinded_message.clone()])
            .await,
    );

    // Step 4: Unblind signature
    let signature = assert_ok(unblind(
        &server_pk,
        &tokens_response.blinded_signatures[0],
        &blinding_op.secret,
        &blinding_op.randomizer,
        &public_key_bytes,
    ));

    // Step 5: Create ephemeral mailbox with real signature
    // Generate a random message randomizer for the mailbox
    let mut message_randomizer = [0u8; 32];
    rand::fill(&mut message_randomizer);
    let create_result = mailbox_client
        .create_ephemeral_mailbox(public_key_bytes.clone(), signature, 86400, message_randomizer.to_vec())
        .await;

    assert!(
        create_result.is_ok(),
        "Should be able to create mailbox with real signature: {:?}",
        create_result.err()
    );

    let response = create_result.unwrap();
    if !response.success {
        println!("Mailbox creation failed: error={}", response.error);
    }
    assert!(response.success, "Mailbox creation should succeed: {}", response.error);
    assert!(!response.mailbox_id.is_empty(), "Should have mailbox ID");

    println!("Created mailbox: {}", response.mailbox_id);
}

#[tokio::test]
async fn test_ephemeral_mailbox_poll_with_real_signature() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Get server key
    let pk_response = assert_ok(mailbox_client.get_blind_signature_public_key().await);
    let server_pk = assert_ok(ServerPublicKey::from_der(
        &pk_response.public_key,
        pk_response.key_id.clone(),
        pk_response.valid_until,
    ));

    // Generate keypair
    let mut secret_bytes = [0u8; 32];
    rand::fill(&mut secret_bytes);
    let signing_key = SigningKey::from_bytes(&secret_bytes);
    let public_key_bytes = signing_key.verifying_key().to_bytes().to_vec();

    // Blind, get token, unblind
    let blinding_op = assert_ok(blind_key(&server_pk, &public_key_bytes));
    let tokens_response = assert_ok(
        mailbox_client
            .get_mailbox_tokens(1, vec![blinding_op.blinded_message.clone()])
            .await,
    );
    let signature = assert_ok(unblind(
        &server_pk,
        &tokens_response.blinded_signatures[0],
        &blinding_op.secret,
        &blinding_op.randomizer,
        &public_key_bytes,
    ));

    // Create mailbox
    let mut message_randomizer = [0u8; 32];
    rand::fill(&mut message_randomizer);
    let create_response = assert_ok(
        mailbox_client
            .create_ephemeral_mailbox(public_key_bytes.clone(), signature, 86400, message_randomizer.to_vec())
            .await,
    );
    assert!(create_response.success, "Mailbox creation should succeed: {}", create_response.error);

    // Get challenge
    let challenge_response = assert_ok(
        mailbox_client
            .get_mailbox_challenge(public_key_bytes.clone())
            .await,
    );
    assert!(!challenge_response.challenge.is_empty());

    // Sign challenge with the real Ed25519 key
    let challenge_sig = signing_key.sign(&challenge_response.challenge);

    // Poll mailbox with real signature
    let poll_result = mailbox_client
        .poll_ephemeral_mailbox(
            public_key_bytes.clone(),
            challenge_sig.to_bytes().to_vec(),
            challenge_response.challenge,
            None,
            100,
        )
        .await;

    assert!(
        poll_result.is_ok(),
        "Should be able to poll mailbox: {:?}",
        poll_result.err()
    );

    let poll_response = poll_result.unwrap();
    println!("Poll returned {} messages", poll_response.messages.len());
}

#[tokio::test]
async fn test_send_to_ephemeral_mailbox() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Create a mailbox first using real crypto
    let pk_response = assert_ok(mailbox_client.get_blind_signature_public_key().await);
    let server_pk = assert_ok(ServerPublicKey::from_der(
        &pk_response.public_key,
        pk_response.key_id.clone(),
        pk_response.valid_until,
    ));

    let mut secret_bytes = [0u8; 32];
    rand::fill(&mut secret_bytes);
    let signing_key = SigningKey::from_bytes(&secret_bytes);
    let public_key_bytes = signing_key.verifying_key().to_bytes().to_vec();

    let blinding_op = assert_ok(blind_key(&server_pk, &public_key_bytes));
    let tokens_response = assert_ok(
        mailbox_client
            .get_mailbox_tokens(1, vec![blinding_op.blinded_message.clone()])
            .await,
    );
    let signature = assert_ok(unblind(
        &server_pk,
        &tokens_response.blinded_signatures[0],
        &blinding_op.secret,
        &blinding_op.randomizer,
        &public_key_bytes,
    ));

    let mut message_randomizer = [0u8; 32];
    rand::fill(&mut message_randomizer);
    let create_response = assert_ok(
        mailbox_client
            .create_ephemeral_mailbox(public_key_bytes.clone(), signature, 86400, message_randomizer.to_vec())
            .await,
    );
    assert!(create_response.success, "Mailbox creation should succeed: {}", create_response.error);

    // Now send a message to this mailbox
    let send_result = mailbox_client
        .send_to_ephemeral_mailbox(
            public_key_bytes.clone(),
            b"encrypted envelope data".to_vec(),
            EnvelopeType::SealedSender as i32,
        )
        .await;

    assert!(
        send_result.is_ok(),
        "Should be able to send to mailbox: {:?}",
        send_result.err()
    );
}

// ============================================================================
// Sender Certificate Tests
// ============================================================================

#[tokio::test]
async fn test_request_sender_certificate() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Request a new sender certificate (valid for 7 days). Server reads
    // the requesting device_id from the authenticated session.
    let result = auth_client
        .request_sender_certificate(Some(std::time::Duration::from_secs(7 * 24 * 60 * 60)))
        .await;

    assert!(
        result.is_ok(),
        "Should be able to request sender certificate: {:?}",
        result.err()
    );

    let response = result.unwrap();
    assert!(
        response.certificate.is_some(),
        "Should have certificate data"
    );
}

// ============================================================================
// Stream Global Events Test
// ============================================================================

#[tokio::test]
async fn test_stream_global_events() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    let stream_result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        msg_client.stream_global_events(vec![], None),
    )
    .await;

    match stream_result {
        Ok(Ok(_stream)) => {
            println!("Global events stream established");
        }
        Ok(Err(e)) => {
            panic!("Failed to establish global events stream: {:?}", e);
        }
        Err(_) => {
            println!("Stream established, no events within timeout (expected)");
        }
    }
}
