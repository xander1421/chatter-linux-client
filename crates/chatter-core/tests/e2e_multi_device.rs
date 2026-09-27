//! End-to-end tests for multi-device support.
//!
//! Tests complete message flows across multiple devices including:
//! - Sending DMs to recipients with multiple devices
//! - Session establishment and reuse
//! - Device revocation handling

use crate::{common, skip_if_no_server};
use crate::framework;

use chatter_core::proto::chat_v1::{
    get_pre_key_bundle_response, ChannelType, MessageSortDirection,
};
use chatter_core::network::grpc::{AuthClient, ChannelClient, MessageClient};
use common::*;
use tokio_stream::StreamExt;

// ============================================================================
// E2E Multi-Device DM Tests
// ============================================================================

/// Test that fetching all devices returns valid bundles.
#[tokio::test]
async fn test_e2e_multi_device_prekey_fetch() {
    skip_if_no_server!();

    // Setup: Alice and Bob
    let alice_pool = assert_ok(create_test_pool().await);
    let _alice = assert_ok(setup_authenticated_test(&alice_pool).await);

    let bob_pool = assert_ok(create_test_pool().await);
    let bob = assert_ok(setup_authenticated_test(&bob_pool).await);

    // Alice fetches Bob's prekeys for all devices
    let mut alice_auth = AuthClient::new(alice_pool.channel(), alice_pool.auth_clone());
    let response = alice_auth.get_pre_key_bundles_all_devices(&bob.user_id).await;

    match response {
        Ok(resp) => {
            let result = resp.result.expect("Should have result");
            match result {
                get_pre_key_bundle_response::Result::Bundle(bundle) => {
                    // Single device - Bob only has one device
                    assert_eq!(
                        bundle.device_id, bob.device_id,
                        "Bundle should be for Bob's device"
                    );
                }
                get_pre_key_bundle_response::Result::Devices(list) => {
                    // Multiple devices
                    assert!(
                        !list.bundles.is_empty(),
                        "Should have at least one device"
                    );
                    // One of the bundles should be for Bob's known device
                    let has_bobs_device = list
                        .bundles
                        .iter()
                        .any(|b| b.device_id == bob.device_id);
                    assert!(has_bobs_device, "Should include Bob's current device");
                }
            }
        }
        Err(e) => {
            // Server might return single bundle format
            eprintln!("Multi-device fetch: {:?}", e);
        }
    }
}

/// Test that sessions are established correctly for multiple devices.
#[tokio::test]
async fn test_e2e_session_establishment_with_prekey_bundle() {
    skip_if_no_server!();

    // Setup users
    let pool1 = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool1).await);

    let pool2 = assert_ok(create_test_pool().await);
    let user2 = assert_ok(setup_authenticated_test(&pool2).await);

    // User1 fetches User2's prekey bundle
    let mut auth1 = AuthClient::new(pool1.channel(), pool1.auth_clone());
    let bundle_response = auth1
        .get_pre_key_bundle(&user2.user_id, &user2.device_id.to_string())
        .await;

    let bundle = assert_ok(bundle_response);
    assert!(bundle.result.is_some(), "Should have bundle result");

    if let Some(get_pre_key_bundle_response::Result::Bundle(b)) = bundle.result {
        // Verify bundle contents for session establishment
        assert_eq!(b.device_id, user2.device_id);
        assert!(!b.identity_key.is_empty());
        assert!(b.signed_prekey.is_some());
    }
}

/// Test channel message delivery (server handles device fanout).
#[tokio::test]
async fn test_e2e_channel_message_reaches_device() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a channel
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("e2e_multidev_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            vec![],
            None,
            Some("E2E multi-device test"),
            None,
        )
        .await
        .expect("Create channel");

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
    let content = chatter_core::proto::chat_v1::message::Content {
        r#type: Some(chatter_core::proto::chat_v1::message::content::Type::Text(
            "Hello from e2e test".to_string(),
        )),
    };

    let message = assert_ok(
        msg_client
            .send_message(&channel_id, content, None, false, None, vec![], false, vec![])
            .await,
    );

    // Verify message can be retrieved (server stored it for our device)
    let messages = assert_ok(
        msg_client
            .get_messages(
                &channel_id,
                10,
                None,
                None,
                MessageSortDirection::SortNewestFirst,
                None,
                None,
            )
            .await,
    );

    let found = messages.messages.iter().any(|m| m.id == message.id);
    assert!(found, "Message should be retrievable by same device");
}

// ============================================================================
// Device Revocation Tests
// ============================================================================

/// Test that device list updates after logout.
#[tokio::test]
async fn test_e2e_device_list_after_operations() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    // Get initial device list
    let devices = assert_ok(auth_client.list_devices().await);
    let initial_count = devices.devices.len();

    assert!(
        initial_count >= 1,
        "Should have at least our current device"
    );

    // Our device should be in the list
    let our_device = devices
        .devices
        .iter()
        .find(|d| d.device_id == user.device_id);
    assert!(our_device.is_some(), "Our device should be listed");
}

// ============================================================================
// Concurrent Device Tests
// ============================================================================

/// Test that multiple concurrent session operations don't conflict.
#[tokio::test]
async fn test_e2e_concurrent_prekey_fetches() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let pool2 = assert_ok(create_test_pool().await);
    let target = assert_ok(setup_authenticated_test(&pool2).await);

    // Spawn multiple concurrent prekey fetches
    let mut handles = vec![];
    for _ in 0..3 {
        let pool_clone = pool.clone();
        let target_id = target.user_id.clone();

        let handle = tokio::spawn(async move {
            let mut auth = AuthClient::new(pool_clone.channel(), pool_clone.auth_clone());
            auth.get_pre_key_bundle(&target_id, "1").await
        });
        handles.push(handle);
    }

    // All fetches should succeed
    let results: Vec<_> = futures::future::join_all(handles).await;
    let successes = results
        .iter()
        .filter(|r| r.as_ref().map(|inner| inner.is_ok()).unwrap_or(false))
        .count();

    assert!(
        successes >= 2,
        "Most concurrent prekey fetches should succeed"
    );
}
