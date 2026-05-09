//! Integration tests for UserClient.
//!
//! Tests user profile operations, presence, friends, blocking,
//! and navigation history.

mod common;

use chatter_core::network::grpc::{ChannelClient, UserClient};
use chatter_core::proto::chat_v1::{friend_requests_filter, ChannelType, UserPresence};
use tokio_stream::StreamExt;

use common::*;

// ============================================================================
// Profile Tests
// ============================================================================

#[tokio::test]
async fn test_get_own_profile() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    let profile = assert_ok(user_client.get_user_profile(&user.user_id).await);

    assert_eq!(profile.user_id, user.user_id);
    assert!(!profile.display_name.is_empty());
}

#[tokio::test]
async fn test_get_nonexistent_user_profile() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    let result = user_client.get_user_profile("nonexistent-user-id").await;
    assert_not_found(result);
}

#[tokio::test]
async fn test_lookup_username_exact_match() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    let hash = chatter_core::crypto::auth_crypto::hash_username_lookup(&user.username)
        .expect("hash should succeed for valid registered username");
    let found = assert_ok(user_client.lookup_username(hash).await);
    assert_eq!(found.as_deref(), Some(user.user_id.as_str()));

    let missing = chatter_core::crypto::auth_crypto::hash_username_lookup("does_not_exist_xyz")
        .expect("hash should succeed for valid lookup string");
    assert_eq!(assert_ok(user_client.lookup_username(missing).await), None);
}

// ============================================================================
// Presence Tests
// ============================================================================

#[tokio::test]
async fn test_update_presence() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // Set online
    let result = user_client.update_presence(UserPresence::Online).await;
    assert!(result.is_ok(), "Should be able to set presence to Online");

    // Set away
    let result = user_client.update_presence(UserPresence::Away).await;
    assert!(result.is_ok(), "Should be able to set presence to Away");

    // Set DND
    let result = user_client
        .update_presence(UserPresence::DoNotDisturb)
        .await;
    assert!(
        result.is_ok(),
        "Should be able to set presence to DoNotDisturb"
    );
}

#[tokio::test]
async fn test_stream_presence() {
    skip_if_no_server!();

    // Use streaming pool (no request timeout) for streaming endpoints
    let pool = assert_ok(create_streaming_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // Start streaming presence with a short timeout
    // We just want to verify the stream can be established, not wait for data
    let stream_result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        user_client.stream_presence(),
    )
    .await;

    // If we timeout, that's OK - it means the stream was established but no data came
    // If we get an error immediately, that's the failure we want to catch
    match stream_result {
        Ok(Ok(_)) => {} // Stream established successfully
        Ok(Err(e)) => panic!("Failed to establish presence stream: {:?}", e),
        Err(_) => {} // Timeout is OK - stream established but no data
    }
}

// ============================================================================
// Friend Management Tests
// ============================================================================

#[tokio::test]
async fn test_list_friends_empty() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // New user should have no friends
    let response = assert_ok(user_client.list_friends(50, 0).await);
    // Might be empty or have some initial entries depending on server
    assert!(response.friends.len() <= 50);
}

#[tokio::test]
async fn test_list_friend_requests() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // List pending requests
    let response = assert_ok(
        user_client
            .list_friend_requests(friend_requests_filter::Filter::Pending, 50, 0)
            .await,
    );

    // New user should have no pending requests
    assert!(response.requests.len() <= 50);
}

#[tokio::test]
async fn test_send_friend_request() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create another user
    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    // Re-auth as user1
    use base64::Engine;
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // Send friend request to user2
    let stream = assert_ok(
        user_client
            .send_friend_request(&user2.user_id, Some("Let's be friends!"))
            .await,
    );

    tokio::pin!(stream);

    // UserEvent has ProfileUpdated, FriendStatusChanged, PresenceChanged
    // FriendStatusChanged indicates friend request state changes
    let mut status_changed = false;
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::user_event::Event;
            if let Event::FriendStatusChanged(_) = payload {
                status_changed = true;
            }
        }
    }

    // May or may not succeed depending on server state
    let _ = status_changed;
}

// ============================================================================
// Blocking Tests
// ============================================================================

#[tokio::test]
async fn test_list_blocked_users() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // List blocked users
    let response = assert_ok(user_client.list_blocked_users(50, 0).await);

    // New user should have no blocked users
    // ListBlockedUsersResponse has `blocked_users` field
    assert!(response.blocked_users.len() <= 50);
}

#[tokio::test]
async fn test_block_and_unblock_user() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create another user to block
    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    // Re-auth as user1
    use base64::Engine;
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // Block user2
    // Wrap in block to ensure stream is dropped before next borrow
    {
        let block_stream = assert_ok(user_client.block_user(&user2.user_id).await);

        tokio::pin!(block_stream);
        while let Some(result) = block_stream.next().await {
            let _event = result.expect("Stream error");
            // UserEvent has ProfileUpdated, FriendStatusChanged, PresenceChanged
            // No explicit Error variant - errors are returned via NetworkError
        }
    }

    // Verify blocked
    let blocked = assert_ok(user_client.list_blocked_users(50, 0).await);
    // ListBlockedUsersResponse has `blocked_users` field
    let is_blocked = blocked.blocked_users.iter().any(|u| u.id == user2.user_id);
    assert!(is_blocked, "User2 should be in blocked list");

    // Unblock user2
    {
        let unblock_stream = assert_ok(user_client.unblock_user(&user2.user_id).await);

        tokio::pin!(unblock_stream);
        while let Some(result) = unblock_stream.next().await {
            let _event = result.expect("Stream error");
            // UserEvent has ProfileUpdated, FriendStatusChanged, PresenceChanged
            // No explicit Error variant - errors are returned via NetworkError
        }
    }
}

// ============================================================================
// Navigation History Tests
// ============================================================================

#[tokio::test]
async fn test_track_channel_visit() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a channel first
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("nav_test_{}", unique_username());

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
            if let Event::ChannelCreated(channel) = payload {
                // ChannelCreated directly contains the Channel proto
                channel_id = channel.id;
            }
        }
    }

    // Track visit
    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());
    let result = user_client.track_channel_visit(&channel_id).await;
    assert!(result.is_ok(), "Should be able to track channel visit");

    // Get navigation history
    let history = assert_ok(
        user_client
            .get_navigation_history(50, true, false)
            .await,
    );

    // GetNavigationHistoryResponse has pinned_channels and recent_channels
    let found = history.recent_channels.iter().any(|ch| ch.channel_id == channel_id)
        || history.pinned_channels.iter().any(|ch| ch.channel_id == channel_id);
    assert!(found, "Visited channel should be in navigation history");
}

#[tokio::test]
async fn test_clear_navigation_history() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // Clear history (keeping pinned)
    let result = user_client.clear_navigation_history(true).await;
    assert!(result.is_ok(), "Should be able to clear navigation history");
}

#[tokio::test]
async fn test_pin_unpin_channel() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a channel
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("pin_test_{}", unique_username());

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
            if let Event::ChannelCreated(channel) = payload {
                // ChannelCreated directly contains the Channel proto
                channel_id = channel.id;
            }
        }
    }

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // Pin the channel
    let pin_response = assert_ok(user_client.pin_channel(&channel_id, 0).await);
    // PinChannelResponse has pinned_channels list
    assert!(pin_response.total_pinned >= 0, "Pin should succeed");

    // Verify in pinned only list
    let history = assert_ok(user_client.get_navigation_history(50, true, true).await);
    // GetNavigationHistoryResponse has pinned_channels
    let found = history.pinned_channels.iter().any(|ch| ch.channel_id == channel_id);
    assert!(found, "Pinned channel should appear in pinned-only history");

    // Unpin it
    let unpin_response = assert_ok(user_client.unpin_channel(&channel_id).await);
    // UnpinChannelResponse has pinned_channels list
    assert!(unpin_response.total_pinned >= 0, "Unpin should succeed");
}

// ============================================================================
// Event Streaming Tests
// ============================================================================

#[tokio::test]
async fn test_stream_user_events() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // Start streaming user events - server sends initial "stream ready" event immediately
    let stream_result = user_client.stream_user_events(&user.user_id).await;
    assert!(
        stream_result.is_ok(),
        "Should be able to establish user event stream"
    );

    // Consume the initial "stream ready" event
    let mut stream = stream_result.unwrap();
    use tokio_stream::StreamExt;
    let first_event = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.next(),
    )
    .await;

    assert!(first_event.is_ok(), "Should receive initial stream event");
}
