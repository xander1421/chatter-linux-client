//! Integration tests for multi-device support.
//!
//! Tests device management, session synchronization, and multi-device messaging.

mod common;

use chatter_core::network::grpc::{AuthClient, ChannelClient, MessageClient};
use chatter_core::proto::chat_v1::{
    message::content::Type as ContentType, message::Content, ChannelType, MessageSortDirection,
};
use libsignal_protocol::{kem, IdentityKeyPair, KeyPair};
use tokio_stream::StreamExt;

use common::*;

// ============================================================================
// Device Registration Tests
// ============================================================================

#[tokio::test]
async fn test_register_second_device() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Login from a "second device"
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Generate new device keys
    let identity_key_pair = IdentityKeyPair::generate(&mut rand::rng());
    let identity_public_key = identity_key_pair.identity_key().serialize();

    let response = auth_client
        .login(
            &user.username,
            &user.password_hash,
            "second-test-device",
            identity_public_key.as_ref(),
            &random_device_auth_token(),
            None, // New device ID
        )
        .await;

    match response {
        Ok(resp) => {
            let profile = resp.user_profile.expect("Should have profile");
            assert_eq!(profile.user_id, user.user_id, "Should be same user");

            // Device ID should be different from first device
            // (Login response contains session info, not device ID directly)
        }
        Err(e) => {
            // Some servers might require device linking flow
            eprintln!("Second device login result: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_list_multiple_devices() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // List devices - should have at least one (our current device)
    let response = assert_ok(auth_client.list_devices().await);

    assert!(
        !response.devices.is_empty(),
        "Should have at least one device"
    );

    // Find our device
    let our_device = response
        .devices
        .iter()
        .find(|d| d.device_id == user.device_id);

    assert!(our_device.is_some(), "Should find our device in list");
}

#[tokio::test]
async fn test_list_active_sessions() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let response = assert_ok(auth_client.list_sessions().await);

    // Should have at least one active session (our current session)
    assert!(
        !response.sessions.is_empty(),
        "Should have at least one active session"
    );

    // Our session should be listed
    let our_session = response
        .sessions
        .iter()
        .find(|s| s.device_id == user.device_id);

    assert!(our_session.is_some(), "Should find our session in list");
}

// ============================================================================
// Device Logout Tests
// ============================================================================

#[tokio::test]
async fn test_logout_specific_device() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Logout our device
    let result = auth_client.logout(user.device_id).await;
    assert!(result.is_ok(), "Logout should succeed");

    // After logout, authenticated requests should fail
    // Clear and don't re-set auth
    pool.auth().clear();

    let mut new_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let devices_result = new_client.list_devices().await;

    assert_unauthenticated(devices_result);
}

// ============================================================================
// Prekey Management Per Device Tests
// ============================================================================

#[tokio::test]
async fn test_upload_prekeys_for_device() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Generate new prekey bundle. Reuse the registered identity key pair —
    // server enforces "all devices for a user share the same identity key"
    // (signal_protocol.go::StoreSignalPreKeyBundle), so a fresh keypair
    // would be rejected with an identity-key-mismatch error.
    use chatter_core::proto::chat_v1::signal_pre_key_bundle::{
        KyberPreKeyRecord, PreKeyRecord, SignedPreKeyRecord,
    };
    use chatter_core::proto::chat_v1::SignalPreKeyBundle;

    let identity_key_pair = IdentityKeyPair::try_from(user.identity_key_pair_bytes.as_slice())
        .expect("registered identity key pair should deserialize");
    let identity_public_key = identity_key_pair.identity_key().serialize();

    let signed_prekey_pair = KeyPair::generate(&mut rand::rng());
    let signed_prekey_signature = identity_key_pair
        .private_key()
        .calculate_signature(&signed_prekey_pair.public_key.serialize(), &mut rand::rng())
        .expect("Signature should succeed");

    let one_time_prekey = KeyPair::generate(&mut rand::rng());

    // PQXDH is mandatory server-side: a bundle without a kyber prekey is
    // rejected with InvalidRequest. Sign it with the identity key like the
    // production path (services/signal/actor.rs) does.
    let kyber_pair = kem::KeyPair::generate(kem::KeyType::Kyber1024, &mut rand::rng());
    let kyber_signature = identity_key_pair
        .private_key()
        .calculate_signature(&kyber_pair.public_key.serialize(), &mut rand::rng())
        .expect("kyber signature should succeed");

    let bundle = SignalPreKeyBundle {
        identity_key: identity_public_key.as_ref().to_vec(),
        registration_id: rand::random::<u32>() % 16380 + 1,
        device_id: user.device_id,
        signed_prekey: Some(SignedPreKeyRecord {
            id: 10,
            public_key: signed_prekey_pair.public_key.serialize().to_vec(),
            signature: signed_prekey_signature.into_vec(),
            // Chatter-owned wire field: nanoseconds (server does time.Unix(0, ts);
            // see internal/utils/crypto/signal_validation.go::checkPreKeyTime).
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 100,
            public_key: one_time_prekey.public_key.serialize().to_vec(),
        }),
        kyber_prekey: Some(KyberPreKeyRecord {
            id: 1,
            public_key: kyber_pair.public_key.serialize().to_vec(),
            signature: kyber_signature.into_vec(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
        }),
        unrestricted_unidentified_access: Some(false),
    };

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let result = auth_client.upload_pre_key_bundle(bundle).await;

    assert!(result.is_ok(), "Should be able to upload prekey bundle: {:?}", result.err());
}

#[tokio::test]
async fn test_get_prekey_stats_for_device() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let response = assert_ok(auth_client.get_pre_key_stats(user.device_id).await);

    assert_eq!(response.device_id, user.device_id);
    // Prekey count can be 0 or more depending on what was uploaded
}

// ============================================================================
// Cross-Device Message Sync Tests
// ============================================================================

#[tokio::test]
async fn test_messages_visible_across_devices() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a channel
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("sync_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            vec![],
            None,
            Some("Multi-device sync test"),
            None,
        )
        .await
        .expect("Failed to create channel");

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(ch) = payload {
                channel_id = ch.id;
            }
        }
    }

    // Send a message
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let content = Content {
        r#type: Some(ContentType::Text("Message from device 1".to_string())),
    };

    let message = assert_ok(
        msg_client
            .send_message(
                &channel_id,
                content,
                None,
                false,
                None,
                None,
                vec![],
                false,
                vec![],
            )
            .await,
    );

    // "Second device" should be able to see the message
    // (In this test, it's the same auth but simulates device sync)
    let messages = assert_ok(
        msg_client
            .get_messages(
                &channel_id,
                50,
                None,
                None,
                MessageSortDirection::SortNewestFirst,
                None,
                None,
            )
            .await,
    );

    let found = messages.messages.iter().any(|m| m.id == message.id);
    assert!(found, "Message should be retrievable (simulating second device)");
}

// ============================================================================
// Sender Certificate Tests
// ============================================================================

#[tokio::test]
async fn test_sender_certificate_per_device() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Request sender certificate for this device. Server reads device_id
    // from the authenticated session, so we don't pass it explicitly.
    let _ = user.device_id;
    let response = assert_ok(
        auth_client
            .request_sender_certificate(None)
            .await,
    );

    assert!(
        response.certificate.is_some(),
        "Should receive sender certificate"
    );

    let cert = response.certificate.unwrap();
    assert!(!cert.certificate.is_empty(), "Certificate data should not be empty");
}

// ============================================================================
// Device Revocation Tests
// ============================================================================

#[tokio::test]
async fn test_revoke_device_session() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Get current sessions
    let sessions = assert_ok(auth_client.list_sessions().await);
    let initial_count = sessions.sessions.len();

    // Try to revoke a session (if there are multiple)
    // In a real scenario, we'd have multiple devices to test with
    // For now, just verify the API works
    if initial_count > 1 {
        // Would revoke another device's session here
    }
}

// ============================================================================
// Concurrent Device Operations Tests
// ============================================================================

#[tokio::test]
async fn test_concurrent_device_operations() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create channel
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("concurrent_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create channel");

    tokio::pin!(stream);
    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(ch) = payload {
                channel_id = ch.id;
            }
        }
    }

    // Simulate concurrent message sends (from same "device" in this test)
    let mut handles = vec![];

    for i in 0..5 {
        let pool_clone = pool.clone();
        let channel_id_clone = channel_id.clone();

        let handle = tokio::spawn(async move {
            let mut msg_client = MessageClient::new(pool_clone.channel(), pool_clone.auth_clone());
            let content = Content {
                r#type: Some(ContentType::Text(format!("Concurrent message {}", i))),
            };

            msg_client
                .send_message(
                    &channel_id_clone,
                    content,
                    None,
                    false,
                    None,
                    None,
                    vec![],
                    false,
                    vec![],
                )
                .await
        });

        handles.push(handle);
    }

    // Wait for all to complete
    let results: Vec<_> = futures::future::join_all(handles).await;

    // Count successes
    let successes = results
        .iter()
        .filter(|r| r.as_ref().map(|inner| inner.is_ok()).unwrap_or(false))
        .count();

    assert!(successes >= 3, "Most concurrent sends should succeed");

    // Verify messages were stored
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let messages = assert_ok(
        msg_client
            .get_messages(
                &channel_id,
                50,
                None,
                None,
                MessageSortDirection::SortNewestFirst,
                None,
                None,
            )
            .await,
    );

    assert!(
        messages.messages.len() >= successes,
        "Should have at least {} messages",
        successes
    );
}

// ============================================================================
// Multi-Device Prekey Fetch Tests
// ============================================================================

#[tokio::test]
async fn test_fetch_prekeys_all_devices() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a second test user (target)
    let pool2 = assert_ok(create_test_pool().await);
    let target = assert_ok(setup_authenticated_test(&pool2).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Fetch prekeys for all devices using wildcard
    let response = auth_client
        .get_pre_key_bundles_all_devices(&target.user_id)
        .await;

    match response {
        Ok(resp) => {
            // Response should contain either Bundle or Devices
            assert!(
                resp.result.is_some(),
                "Should have a result (Bundle or Devices)"
            );

            match resp.result.unwrap() {
                chatter_core::proto::chat_v1::get_pre_key_bundle_response::Result::Bundle(b) => {
                    // Single device - should have valid device_id
                    assert!(b.device_id > 0, "Device ID should be positive");
                    assert!(!b.identity_key.is_empty(), "Should have identity key");
                }
                chatter_core::proto::chat_v1::get_pre_key_bundle_response::Result::Devices(
                    list,
                ) => {
                    // Multiple devices
                    assert!(
                        !list.bundles.is_empty(),
                        "Should have at least one device bundle"
                    );
                    for bundle in &list.bundles {
                        assert!(bundle.device_id > 0, "Device ID should be positive");
                    }
                }
            }
        }
        Err(e) => {
            // Some servers might not support wildcard device query
            eprintln!("Multi-device prekey fetch result: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_prekey_bundles_have_required_fields() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create target user
    let pool2 = assert_ok(create_test_pool().await);
    let target = assert_ok(setup_authenticated_test(&pool2).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Fetch single device prekey
    let response = auth_client
        .get_pre_key_bundle(&target.user_id, "1")
        .await;

    if let Ok(resp) = response {
        if let Some(chatter_core::proto::chat_v1::get_pre_key_bundle_response::Result::Bundle(
            bundle,
        )) = resp.result
        {
            // Verify bundle has required fields for PQXDH
            assert!(!bundle.identity_key.is_empty(), "Should have identity key");
            assert!(bundle.signed_prekey.is_some(), "Should have signed prekey");
            assert!(bundle.kyber_prekey.is_some(), "Should have Kyber prekey (PQXDH)");
            assert!(bundle.registration_id > 0, "Should have registration ID");
        }
    }
}

// ============================================================================
// Multi-Device Session Tests
// ============================================================================

#[tokio::test]
async fn test_session_count_matches_device_count() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // List our devices
    let devices_response = assert_ok(auth_client.list_devices().await);
    let device_count = devices_response.devices.len();

    // Each device should have its own prekey bundle
    // (This test verifies the server tracks devices correctly)
    assert!(
        device_count >= 1,
        "Should have at least one device registered"
    );

    // Verify we can get prekey stats for our device
    let stats = assert_ok(auth_client.get_pre_key_stats(user.device_id).await);
    assert_eq!(
        stats.device_id, user.device_id,
        "Stats should be for our device"
    );
}
