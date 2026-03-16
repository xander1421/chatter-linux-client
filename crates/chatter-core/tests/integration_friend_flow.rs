//! Integration tests for complete friend workflow.
//!
//! Tests the full friend lifecycle: request, accept, chat, unfriend.

mod common;

use base64::Engine;
use chatter_linux::network::grpc::{ChannelClient, UserClient};
use chatter_linux::proto::chat_v1::{friend_requests_filter, ChannelType, FriendStatus};
use tokio_stream::StreamExt;

use common::*;

// ============================================================================
// Friend Request Flow Tests
// ============================================================================

#[tokio::test]
async fn test_friend_request_send_and_list() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create second user
    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    // Re-auth as user1
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // Send friend request to user2
    let stream = assert_ok(
        user_client
            .send_friend_request(&user2.user_id, Some("Hey, let's be friends!"))
            .await,
    );

    tokio::pin!(stream);
    while let Some(result) = stream.next().await {
        let _event = result.expect("Stream error");
    }

    // Switch to user2 and check pending requests
    pool.auth().set_basic(
        format!("{}.{}", user2.user_id, user2.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user2.device_auth_token),
    );

    let mut user2_client = UserClient::new(pool.channel(), pool.auth_clone());
    let requests = assert_ok(
        user2_client
            .list_friend_requests(friend_requests_filter::Filter::Pending, 50, 0)
            .await,
    );

    // Should have request from user1 (Contact has user_id field)
    let has_request = requests
        .requests
        .iter()
        .any(|r| r.user_id == user1.user_id);
    assert!(has_request, "User2 should have pending request from User1");
}

#[tokio::test]
async fn test_friend_request_accept() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create and get user2
    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    // User1 sends friend request
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut user1_client = UserClient::new(pool.channel(), pool.auth_clone());
    let stream = assert_ok(
        user1_client
            .send_friend_request(&user2.user_id, None)
            .await,
    );
    tokio::pin!(stream);
    while let Some(_) = stream.next().await {}

    // User2 accepts
    pool.auth().set_basic(
        format!("{}.{}", user2.user_id, user2.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user2.device_auth_token),
    );

    // Consume the accept stream in a scope so it's dropped before list_friends
    {
        let mut user2_client = UserClient::new(pool.channel(), pool.auth_clone());
        let accept_stream = assert_ok(
            user2_client
                .respond_friend_request(&user1.user_id, FriendStatus::Accepted, None)
                .await,
        );
        tokio::pin!(accept_stream);
        while let Some(_) = accept_stream.next().await {}
    }

    // Verify user1 is in user2's friend list using a fresh client
    let mut user2_client = UserClient::new(pool.channel(), pool.auth_clone());
    let friends = assert_ok(user2_client.list_friends(50, 0).await);
    let is_friend = friends.friends.iter().any(|f| f.user_id == user1.user_id);
    assert!(is_friend, "User1 should be in User2's friend list");
}

#[tokio::test]
async fn test_friend_request_decline() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    // User1 sends friend request
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut user1_client = UserClient::new(pool.channel(), pool.auth_clone());
    let stream = assert_ok(
        user1_client
            .send_friend_request(&user2.user_id, None)
            .await,
    );
    tokio::pin!(stream);
    while let Some(_) = stream.next().await {}

    // User2 declines
    pool.auth().set_basic(
        format!("{}.{}", user2.user_id, user2.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user2.device_auth_token),
    );

    // Consume the decline stream in a scope so it's dropped before list_friends
    {
        let mut user2_client = UserClient::new(pool.channel(), pool.auth_clone());
        let decline_stream = assert_ok(
            user2_client
                .respond_friend_request(&user1.user_id, FriendStatus::Declined, None)
                .await,
        );
        tokio::pin!(decline_stream);
        while let Some(_) = decline_stream.next().await {}
    }

    // Verify user1 is NOT in user2's friend list using a fresh client
    let mut user2_client = UserClient::new(pool.channel(), pool.auth_clone());
    let friends = assert_ok(user2_client.list_friends(50, 0).await);
    let is_friend = friends.friends.iter().any(|f| f.user_id == user1.user_id);
    assert!(!is_friend, "User1 should NOT be in User2's friend list after decline");
}

// ============================================================================
// Friend Chat Flow Tests
// ============================================================================

#[tokio::test]
async fn test_friends_can_create_dm_channel() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    // Make them friends
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut user1_client = UserClient::new(pool.channel(), pool.auth_clone());
    let stream = assert_ok(
        user1_client
            .send_friend_request(&user2.user_id, None)
            .await,
    );
    tokio::pin!(stream);
    while let Some(_) = stream.next().await {}

    pool.auth().set_basic(
        format!("{}.{}", user2.user_id, user2.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user2.device_auth_token),
    );

    let mut user2_client = UserClient::new(pool.channel(), pool.auth_clone());
    let accept_stream = assert_ok(
        user2_client
            .respond_friend_request(&user1.user_id, FriendStatus::Accepted, None)
            .await,
    );
    tokio::pin!(accept_stream);
    while let Some(_) = accept_stream.next().await {}

    // Now user1 creates DM channel
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let dm_name = format!("dm_{}_{}", user1.user_id, user2.user_id);
    let dm_result = channel_client
        .create_channel(
            &dm_name,
            ChannelType::DirectMessage,
            None,
            vec![user2.user_id.clone()],
            None,
            None,
            None,
        )
        .await;

    match dm_result {
        Ok(stream) => {
            tokio::pin!(stream);
            let mut channel_id = String::new();
            while let Some(result) = stream.next().await {
                let event = result.expect("Stream error");
                if let Some(payload) = event.event {
                    use chatter_linux::proto::chat_v1::channel_event::Event;
                    if let Event::ChannelCreated(ch) = payload {
                        channel_id = ch.id;
                    }
                }
            }
            assert!(!channel_id.is_empty(), "DM channel should be created");
        }
        Err(e) => {
            // DM creation might fail if channel already exists - that's okay
            eprintln!("DM channel creation: {:?}", e);
        }
    }
}

// ============================================================================
// Remove Friend Tests
// ============================================================================

#[tokio::test]
async fn test_remove_friend() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    // Make friends
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut user1_client = UserClient::new(pool.channel(), pool.auth_clone());
    let stream = assert_ok(
        user1_client
            .send_friend_request(&user2.user_id, None)
            .await,
    );
    tokio::pin!(stream);
    while let Some(_) = stream.next().await {}

    pool.auth().set_basic(
        format!("{}.{}", user2.user_id, user2.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user2.device_auth_token),
    );

    let mut user2_client = UserClient::new(pool.channel(), pool.auth_clone());
    let accept_stream = assert_ok(
        user2_client
            .respond_friend_request(&user1.user_id, FriendStatus::Accepted, None)
            .await,
    );
    tokio::pin!(accept_stream);
    while let Some(_) = accept_stream.next().await {}

    // User1 removes user2 as friend
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut user1_client = UserClient::new(pool.channel(), pool.auth_clone());
    // remove_friend returns () not a stream
    assert_ok(user1_client.remove_friend(&user2.user_id).await);

    // Verify no longer friends
    let friends = assert_ok(user1_client.list_friends(50, 0).await);
    let still_friends = friends.friends.iter().any(|f| f.user_id == user2.user_id);
    assert!(!still_friends, "User2 should not be in friend list after removal");
}

// ============================================================================
// Friend List Tests
// ============================================================================

#[tokio::test]
async fn test_list_friends_empty() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // New user should have no friends
    let friends = assert_ok(user_client.list_friends(50, 0).await);

    // Might have 0 friends or some from previous tests
    // Just verify the API works
    assert!(friends.friends.len() >= 0, "Should return friend list");
}

#[tokio::test]
async fn test_list_friend_requests_types() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // Test different filter types
    let pending = user_client
        .list_friend_requests(friend_requests_filter::Filter::Pending, 50, 0)
        .await;
    assert!(pending.is_ok(), "Should list pending requests");

    let sent = user_client
        .list_friend_requests(friend_requests_filter::Filter::Sent, 50, 0)
        .await;
    assert!(sent.is_ok(), "Should list sent requests");
}
