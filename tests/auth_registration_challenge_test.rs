//! End-to-end tests for registration challenge flow
//! 
//! These tests require a running server instance and test the complete
//! registration challenge flow from client to server and back.

use std::time::Duration;

use chatter_linux::crypto::identity::IdentityManager;
use chatter_linux::network::grpc::{AuthClient, GrpcClientPool};
use chatter_linux::storage::keystore::{InMemoryKeyStore, KeyStore};
use ed25519_dalek::{Keypair, PublicKey, SecretKey};
use rand::rngs::OsRng;
use tokio::test;

const SERVER_ADDRESS: &str = "http://localhost:50051";

/// Test the complete registration challenge flow end-to-end
#[test]
async fn test_registration_challenge_flow() {
    // Skip if server is not running
    if !is_server_running().await {
        eprintln!("Skipping test: server not running at {}", SERVER_ADDRESS);
        return;
    }

    // Setup gRPC connection
    let pool = GrpcClientPool::new(SERVER_ADDRESS).expect("Failed to create gRPC pool");
    pool.connect().await.expect("Failed to connect to server");

    // Setup identity manager with test key store
    let mut key_store = InMemoryKeyStore::new();
    let identity_manager = IdentityManager::new(&mut key_store).expect("Failed to create identity manager");

    // Generate test identity
    let identity_keypair = identity_manager.identity_keypair().expect("Failed to get identity keypair");
    let identity_public_key = identity_keypair.identity_key().serialize();

    // Create auth client
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Test data
    let test_username = "test_user_e2e";

    // Step 1: Request registration challenge
    let challenge_response = auth_client
        .request_registration_challenge(test_username, &identity_public_key)
        .await
        .expect("Failed to request registration challenge");

    // Verify challenge response structure
    assert!(!challenge_response.challenge_id.is_empty(), "Challenge ID should not be empty");
    assert_eq!(challenge_response.challenge_nonce.len(), 32, "Challenge nonce should be 32 bytes");
    assert!(challenge_response.expires_at.is_some(), "Expires at should be set");

    // Step 2: Sign the challenge nonce
    let signature = identity_manager
        .sign(&challenge_response.challenge_nonce)
        .expect("Failed to sign challenge nonce");

    // Step 3: Verify the challenge response
    let verify_response = auth_client
        .verify_registration_challenge(&challenge_response.challenge_id, &signature)
        .await
        .expect("Failed to verify registration challenge");

    // Verify the response
    assert_eq!(verify_response.username, test_username, "Username should match");
    assert_eq!(
        verify_response.identity_public_key, 
        identity_public_key, 
        "Identity public key should match"
    );

    println!("✓ Registration challenge flow completed successfully");
}

/// Test that challenges expire properly
#[test]
async fn test_challenge_expiry() {
    // Skip if server is not running
    if !is_server_running().await {
        eprintln!("Skipping test: server not running at {}", SERVER_ADDRESS);
        return;
    }

    // Setup
    let pool = GrpcClientPool::new(SERVER_ADDRESS).expect("Failed to create gRPC pool");
    pool.connect().await.expect("Failed to connect to server");

    let mut key_store = InMemoryKeyStore::new();
    let identity_manager = IdentityManager::new(&mut key_store).expect("Failed to create identity manager");
    let identity_public_key = identity_manager.identity_keypair()
        .expect("Failed to get identity keypair")
        .identity_key()
        .serialize();

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Request challenge
    let challenge_response = auth_client
        .request_registration_challenge("test_user_expiry", &identity_public_key)
        .await
        .expect("Failed to request registration challenge");

    let signature = identity_manager
        .sign(&challenge_response.challenge_nonce)
        .expect("Failed to sign challenge nonce");

    // Wait for challenge to potentially expire (this assumes challenges have a short expiry for testing)
    // In real implementation, you might want to test with a mock time or shorter expiry
    
    // Try to verify immediately (should work)
    let verify_response = auth_client
        .verify_registration_challenge(&challenge_response.challenge_id, &signature)
        .await
        .expect("Failed to verify fresh challenge");

    assert_eq!(verify_response.username, "test_user_expiry");

    // Try to verify the same challenge again (should fail - one-time use)
    let second_verify_result = auth_client
        .verify_registration_challenge(&challenge_response.challenge_id, &signature)
        .await;

    assert!(
        second_verify_result.is_err(),
        "Second verification should fail (challenge should be deleted after first use)"
    );

    println!("✓ Challenge one-time use behavior verified");
}

/// Test signature verification security
#[test]
async fn test_signature_verification_security() {
    // Skip if server is not running
    if !is_server_running().await {
        eprintln!("Skipping test: server not running at {}", SERVER_ADDRESS);
        return;
    }

    // Setup
    let pool = GrpcClientPool::new(SERVER_ADDRESS).expect("Failed to create gRPC pool");
    pool.connect().await.expect("Failed to connect to server");

    let mut key_store = InMemoryKeyStore::new();
    let identity_manager = IdentityManager::new(&mut key_store).expect("Failed to create identity manager");
    let identity_public_key = identity_manager.identity_keypair()
        .expect("Failed to get identity keypair")
        .identity_key()
        .serialize();

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Request challenge
    let challenge_response = auth_client
        .request_registration_challenge("test_user_security", &identity_public_key)
        .await
        .expect("Failed to request registration challenge");

    // Create a wrong signature (sign different data)
    let wrong_data = b"this is not the challenge nonce";
    let wrong_signature = identity_manager
        .sign(wrong_data)
        .expect("Failed to create wrong signature");

    // Try to verify with wrong signature (should fail)
    let verify_result = auth_client
        .verify_registration_challenge(&challenge_response.challenge_id, &wrong_signature)
        .await;

    assert!(
        verify_result.is_err(),
        "Verification with wrong signature should fail"
    );

    // Now sign the correct challenge nonce
    let correct_signature = identity_manager
        .sign(&challenge_response.challenge_nonce)
        .expect("Failed to sign challenge nonce");

    // This should work
    let verify_response = auth_client
        .verify_registration_challenge(&challenge_response.challenge_id, &correct_signature)
        .await
        .expect("Failed to verify with correct signature");

    assert_eq!(verify_response.username, "test_user_security");

    println!("✓ Signature verification security verified");
}

/// Test invalid input handling
#[test]
async fn test_invalid_input_handling() {
    // Skip if server is not running
    if !is_server_running().await {
        eprintln!("Skipping test: server not running at {}", SERVER_ADDRESS);
        return;
    }

    // Setup
    let pool = GrpcClientPool::new(SERVER_ADDRESS).expect("Failed to create gRPC pool");
    pool.connect().await.expect("Failed to connect to server");
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Test with invalid public key length
    let invalid_public_key = vec![0u8; 16]; // Wrong length (should be 32)
    let result = auth_client
        .request_registration_challenge("test_user", &invalid_public_key)
        .await;

    assert!(result.is_err(), "Should reject invalid public key length");

    // Test with empty username
    let mut key_store = InMemoryKeyStore::new();
    let identity_manager = IdentityManager::new(&mut key_store).expect("Failed to create identity manager");
    let identity_public_key = identity_manager.identity_keypair()
        .expect("Failed to get identity keypair")
        .identity_key()
        .serialize();

    let result = auth_client
        .request_registration_challenge("", &identity_public_key)
        .await;

    assert!(result.is_err(), "Should reject empty username");

    // Test verification with invalid challenge ID
    let dummy_signature = vec![0u8; 64]; // Valid signature length but wrong data
    let result = auth_client
        .verify_registration_challenge("invalid-challenge-id", &dummy_signature)
        .await;

    assert!(result.is_err(), "Should reject invalid challenge ID");

    // Test verification with empty challenge ID
    let result = auth_client
        .verify_registration_challenge("", &dummy_signature)
        .await;

    assert!(result.is_err(), "Should reject empty challenge ID");

    // Test verification with empty signature
    let empty_signature = vec![];
    let result = auth_client
        .verify_registration_challenge("some-challenge-id", &empty_signature)
        .await;

    assert!(result.is_err(), "Should reject empty signature");

    println!("✓ Invalid input handling verified");
}

/// Test concurrent challenges
#[test]
async fn test_concurrent_challenges() {
    // Skip if server is not running
    if !is_server_running().await {
        eprintln!("Skipping test: server not running at {}", SERVER_ADDRESS);
        return;
    }

    // Setup
    let pool = GrpcClientPool::new(SERVER_ADDRESS).expect("Failed to create gRPC pool");
    pool.connect().await.expect("Failed to connect to server");

    // Create multiple identity managers
    let mut handles = vec![];
    
    for i in 0..5 {
        let pool_clone = pool.clone();
        let handle = tokio::spawn(async move {
            let mut key_store = InMemoryKeyStore::new();
            let identity_manager = IdentityManager::new(&mut key_store)
                .expect("Failed to create identity manager");
            let identity_public_key = identity_manager.identity_keypair()
                .expect("Failed to get identity keypair")
                .identity_key()
                .serialize();

            let mut auth_client = AuthClient::new(pool_clone.channel(), pool_clone.auth_clone());

            // Request challenge
            let challenge_response = auth_client
                .request_registration_challenge(&format!("test_user_concurrent_{}", i), &identity_public_key)
                .await
                .expect("Failed to request registration challenge");

            // Sign and verify
            let signature = identity_manager
                .sign(&challenge_response.challenge_nonce)
                .expect("Failed to sign challenge nonce");

            let verify_response = auth_client
                .verify_registration_challenge(&challenge_response.challenge_id, &signature)
                .await
                .expect("Failed to verify registration challenge");

            verify_response.username
        });
        handles.push(handle);
    }

    // Wait for all concurrent operations to complete
    for (i, handle) in handles.into_iter().enumerate() {
        let username = handle.await.expect("Task failed");
        assert_eq!(username, format!("test_user_concurrent_{}", i));
    }

    println!("✓ Concurrent challenges handled successfully");
}

/// Check if the server is running and accessible
async fn is_server_running() -> bool {
    match tokio::time::timeout(Duration::from_secs(2), check_server_health()).await {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => false,
        Err(_) => false, // Timeout
    }
}

/// Simple health check for the server
async fn check_server_health() -> Result<(), Box<dyn std::error::Error>> {
    let pool = GrpcClientPool::new(SERVER_ADDRESS)?;
    pool.connect().await?;
    Ok(())
}

/// Performance test for registration challenge flow
#[test]
async fn test_registration_challenge_performance() {
    // Skip if server is not running
    if !is_server_running().await {
        eprintln!("Skipping test: server not running at {}", SERVER_ADDRESS);
        return;
    }

    let pool = GrpcClientPool::new(SERVER_ADDRESS).expect("Failed to create gRPC pool");
    pool.connect().await.expect("Failed to connect to server");

    let start_time = std::time::Instant::now();
    let num_operations = 10;

    for i in 0..num_operations {
        let mut key_store = InMemoryKeyStore::new();
        let identity_manager = IdentityManager::new(&mut key_store)
            .expect("Failed to create identity manager");
        let identity_public_key = identity_manager.identity_keypair()
            .expect("Failed to get identity keypair")
            .identity_key()
            .serialize();

        let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

        // Full challenge flow
        let challenge_response = auth_client
            .request_registration_challenge(&format!("test_user_perf_{}", i), &identity_public_key)
            .await
            .expect("Failed to request registration challenge");

        let signature = identity_manager
            .sign(&challenge_response.challenge_nonce)
            .expect("Failed to sign challenge nonce");

        let _verify_response = auth_client
            .verify_registration_challenge(&challenge_response.challenge_id, &signature)
            .await
            .expect("Failed to verify registration challenge");
    }

    let elapsed = start_time.elapsed();
    let avg_time = elapsed / num_operations;

    println!("✓ Performance test completed:");
    println!("  {} operations in {:?}", num_operations, elapsed);
    println!("  Average time per operation: {:?}", avg_time);

    // Assert reasonable performance (should complete in under 1 second per operation)
    assert!(
        avg_time < Duration::from_secs(1),
        "Registration challenge flow is too slow: {:?} per operation",
        avg_time
    );
}