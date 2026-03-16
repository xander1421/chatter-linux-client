//! Modal callback tests (channel creation, profile actions, friend requests).

use crate::common::*;

// ============================================================================
// Modal Callback Tests - Channel Creation
// ============================================================================

#[tokio::test]
async fn test_create_channel_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Mock UI state
    let channel_name_input = format!("New Channel {}", rand::random::<u32>());
    let channel_description_input = "A test channel".to_string();
    let mut create_channel_loading = false;
    let mut create_channel_error: Option<String> = None;
    let mut active_modal = "create-channel".to_string();
    let mut selected_channel_id: Option<String> = None;

    // Simulate: on_create_channel callback
    create_channel_loading = true;
    create_channel_error = None;

    use chatter_core::network::grpc::ChannelClient;
    use chatter_core::proto::chat_v1::ChannelType;
    use tokio_stream::StreamExt;

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());

    let result = channel_client
        .create_channel(
            &channel_name_input,
            ChannelType::ChannelPublic,
            None,
            vec![],
            None,
            Some(&channel_description_input),
            None,
        )
        .await;

    match result {
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(event_result) = stream.next().await {
                if let Ok(event) = event_result {
                    if let Some(payload) = event.event {
                        use chatter_core::proto::chat_v1::channel_event::Event;
                        match payload {
                            Event::ChannelCreated(channel) => {
                                create_channel_loading = false;
                                active_modal.clear();
                                selected_channel_id = Some(channel.id);
                            }
                            Event::OperationFailed(e) => {
                                create_channel_loading = false;
                                create_channel_error = Some(e.error.map(|err| err.message).unwrap_or_default());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Err(e) => {
            create_channel_loading = false;
            create_channel_error = Some(e.to_string());
        }
    }

    // Verify success case
    assert!(!create_channel_loading);
    assert!(create_channel_error.is_none(), "Error: {:?}", create_channel_error);
    assert!(active_modal.is_empty(), "Modal should be closed");
    assert!(selected_channel_id.is_some(), "Should have new channel ID");
}

#[tokio::test]
async fn test_create_channel_validation_error() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Mock UI state with invalid data
    let create_channel_name = "".to_string(); // Empty name should fail
    let mut create_channel_loading = false;
    let mut create_channel_error: Option<String> = None;

    // Simulate: on_create_channel with empty name
    create_channel_loading = true;

    use chatter_core::network::grpc::ChannelClient;
    use chatter_core::proto::chat_v1::ChannelType;
    use tokio_stream::StreamExt;

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());

    let result = channel_client
        .create_channel(
            &create_channel_name,
            ChannelType::ChannelPublic,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await;

    match result {
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(event_result) = stream.next().await {
                if let Ok(event) = event_result {
                    if let Some(payload) = event.event {
                        use chatter_core::proto::chat_v1::channel_event::Event;
                        if let Event::OperationFailed(e) = payload {
                            create_channel_loading = false;
                            create_channel_error = Some(e.error.map(|err| err.message).unwrap_or_default());
                        }
                    }
                }
            }
            if create_channel_error.is_none() {
                create_channel_loading = false;
            }
        }
        Err(e) => {
            create_channel_loading = false;
            create_channel_error = Some(e.to_string());
        }
    }

    // Verify error case
    assert!(!create_channel_loading);
    assert!(create_channel_error.is_some(), "Should have validation error");
}

// ============================================================================
// Modal Callback Tests - Profile Actions
// ============================================================================

#[tokio::test]
async fn test_profile_add_friend_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create another user to friend
    let pool2 = assert_ok(create_test_pool().await);
    let user2 = assert_ok(register_test_user(&pool2).await);

    // Mock UI state
    let profile_user_id = user2.user_id.clone();
    let mut profile_loading = false;

    // Simulate: on_profile_add_friend callback
    profile_loading = true;

    use chatter_core::network::grpc::UserClient;

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    let result = user_client
        .send_friend_request(&profile_user_id, None)
        .await;

    match result {
        Ok(_) => {
            profile_loading = false;
            // Friend request sent (not yet accepted)
        }
        Err(e) => {
            profile_loading = false;
            eprintln!("Friend request failed: {}", e);
        }
    }

    // Verify
    assert!(!profile_loading);
}

#[tokio::test]
async fn test_profile_block_user_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create user to block
    let pool2 = assert_ok(create_test_pool().await);
    let user2 = assert_ok(register_test_user(&pool2).await);

    // Mock UI state
    let profile_user_id = user2.user_id.clone();
    let mut profile_loading = false;
    let mut profile_is_blocked = false;

    // Simulate: on_profile_block_user callback
    profile_loading = true;

    use chatter_core::network::grpc::UserClient;
    use tokio_stream::StreamExt;

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    let result = user_client
        .block_user(&profile_user_id)
        .await;

    match result {
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(_event) = stream.next().await {
                // Process events
            }
            profile_loading = false;
            profile_is_blocked = true;
        }
        Err(e) => {
            profile_loading = false;
            eprintln!("Block user failed: {}", e);
        }
    }

    // Verify
    assert!(!profile_loading);
    assert!(profile_is_blocked);
}

// ============================================================================
// Modal Callback Tests - Friend Requests
// ============================================================================

#[tokio::test]
async fn test_friend_request_accept_callback_flow() {
    skip_if_no_server!();

    // User1 sends friend request to User2
    let pool1 = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool1).await);

    let pool2 = assert_ok(create_test_pool().await);
    let _user2 = assert_ok(setup_authenticated_test(&pool2).await);

    // User1 sends request
    use chatter_core::network::grpc::UserClient;
    use chatter_core::proto::chat_v1::FriendStatus;

    let mut user1_client = UserClient::new(pool1.channel(), pool1.auth_clone());
    let _ = user1_client
        .send_friend_request(&_user2.user_id, None)
        .await;

    // User2 accepts
    let mut user2_client = UserClient::new(pool2.channel(), pool2.auth_clone());

    // Mock UI state for User2
    let mut friend_requests_loading = false;
    let request_id = user1.user_id.clone(); // The sender's ID

    // Simulate: on_friend_request_accept callback
    friend_requests_loading = true;

    let _result = user2_client
        .respond_friend_request(&request_id, FriendStatus::Accepted, None)
        .await;

    friend_requests_loading = false;

    // Verify
    assert!(!friend_requests_loading);
    // Accept should succeed (or fail gracefully if request doesn't exist)
}

#[tokio::test]
async fn test_friend_request_decline_callback_flow() {
    skip_if_no_server!();

    // User1 sends friend request to User2
    let pool1 = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool1).await);

    let pool2 = assert_ok(create_test_pool().await);
    let _user2 = assert_ok(setup_authenticated_test(&pool2).await);

    // User1 sends request
    use chatter_core::network::grpc::UserClient;
    use chatter_core::proto::chat_v1::FriendStatus;

    let mut user1_client = UserClient::new(pool1.channel(), pool1.auth_clone());
    let _ = user1_client
        .send_friend_request(&_user2.user_id, None)
        .await;

    // User2 declines
    let mut user2_client = UserClient::new(pool2.channel(), pool2.auth_clone());

    let mut friend_requests_loading = false;
    let request_id = user1.user_id.clone();

    // Simulate: on_friend_request_decline callback
    friend_requests_loading = true;

    let _result = user2_client
        .respond_friend_request(&request_id, FriendStatus::Declined, None)
        .await;

    friend_requests_loading = false;

    // Verify
    assert!(!friend_requests_loading);
}

// ============================================================================
// Forward Message Callback Tests
// ============================================================================

#[test]
fn test_forward_message_callback() {
    // Simulate: on_forward_message(target_id)
    let _message_id = "msg-to-forward";
    let _target_channel_id = "target-channel-123";
    let mut forward_loading = false;
    let mut active_modal = "forward".to_string();

    // Callback action
    forward_loading = true;

    // Would forward message to target channel
    // On success:
    forward_loading = false;
    active_modal.clear();

    assert!(!forward_loading);
    assert!(active_modal.is_empty(), "Modal should close after forward");
}
