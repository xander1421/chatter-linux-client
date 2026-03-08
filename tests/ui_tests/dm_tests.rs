//! DM channel and friend request tests.

use crate::common::*;

// ============================================================================
// Friend Request Accept - DM Channel Sidebar Test
// ============================================================================

/// Test that accepting a friend request creates a DM channel visible to both users.
///
/// This test verifies the complete friend request acceptance flow:
/// 1. User1 sends friend request to User2
/// 2. User2 accepts the friend request
/// 3. Server creates a DM channel and returns it in the FriendStatusChanged event
/// 4. Both users should have access to the DM channel with correct display names
#[tokio::test]
async fn test_friend_accept_creates_dm_channel_for_both_sides() {
    skip_if_no_server!();

    // ========== Setup: Create two authenticated users ==========
    let pool1 = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool1).await);

    let pool2 = assert_ok(create_test_pool().await);
    let user2 = assert_ok(setup_authenticated_test(&pool2).await);

    use chatter_linux::network::grpc::{ChannelClient, UserClient};
    use chatter_linux::proto::chat_v1::{FriendStatus, user_event};
    use tokio_stream::StreamExt;

    // ========== Step 1: User1 sends friend request to User2 ==========
    let mut user1_client = UserClient::new(pool1.channel(), pool1.auth_clone());

    let send_result = user1_client
        .send_friend_request(&user2.user_id, Some("Let's be friends!"))
        .await;

    assert!(
        send_result.is_ok(),
        "User1 should be able to send friend request to User2: {:?}",
        send_result.err()
    );

    // ========== Step 2: User2 accepts the friend request ==========
    let mut user2_client = UserClient::new(pool2.channel(), pool2.auth_clone());

    let accept_result = user2_client
        .respond_friend_request(&user1.user_id, FriendStatus::Accepted, None)
        .await;

    assert!(
        accept_result.is_ok(),
        "User2 should be able to accept friend request: {:?}",
        accept_result.err()
    );

    // ========== Step 3: Process the accept response stream to get DM channel ==========
    let mut accept_stream = accept_result.unwrap();
    let mut dm_channel_id: Option<String> = None;
    let mut friend_status_received = false;

    while let Some(event_result) = accept_stream.next().await {
        match event_result {
            Ok(event) => {
                if let Some(user_event::Event::FriendStatusChanged(status_response)) = event.event {
                    friend_status_received = true;

                    // Verify the status is Accepted
                    assert_eq!(
                        status_response.status,
                        FriendStatus::Accepted as i32,
                        "Friend status should be Accepted"
                    );

                    // Verify we got a DM channel ID
                    if !status_response.channel_id.is_empty() {
                        dm_channel_id = Some(status_response.channel_id.clone());
                    }

                    // Verify the user_id in the response is user1's ID
                    assert_eq!(
                        status_response.user_id, user1.user_id,
                        "FriendStatusChanged should reference user1's ID"
                    );
                }
            }
            Err(e) => {
                panic!("Error in accept stream: {:?}", e);
            }
        }
    }

    assert!(friend_status_received, "Should have received FriendStatusChanged event");
    assert!(dm_channel_id.is_some(), "Should have received a DM channel ID");

    let channel_id = dm_channel_id.unwrap();

    // ========== Step 4: Verify User2 can access the DM channel ==========
    let mut channel_client2 = ChannelClient::new(pool2.channel(), pool2.auth_clone());

    let channel2_result = channel_client2.get_channel(&channel_id).await;

    assert!(
        channel2_result.is_ok(),
        "User2 should be able to access the DM channel: {:?}",
        channel2_result.err()
    );

    let channel2 = channel2_result.unwrap();

    // Verify it's a DM channel
    use chatter_linux::proto::chat_v1::ChannelType;
    assert_eq!(
        channel2.r#type,
        ChannelType::DirectMessage as i32,
        "Channel should be a Direct Message type"
    );

    // ========== Step 5: Verify User1 can also access the DM channel ==========
    let mut channel_client1 = ChannelClient::new(pool1.channel(), pool1.auth_clone());

    let channel1_result = channel_client1.get_channel(&channel_id).await;

    assert!(
        channel1_result.is_ok(),
        "User1 should also be able to access the DM channel: {:?}",
        channel1_result.err()
    );

    let channel1 = channel1_result.unwrap();

    // Verify it's the same channel
    assert_eq!(channel1.id, channel2.id, "Both users should see the same channel");
    assert_eq!(
        channel1.r#type,
        ChannelType::DirectMessage as i32,
        "Channel should be a Direct Message type for User1 too"
    );

    // ========== Step 6: Verify both users are members of the channel ==========
    let members1_result = channel_client1.get_channel_members(&channel_id, 10, 0).await;
    assert!(
        members1_result.is_ok(),
        "Should be able to get channel members: {:?}",
        members1_result.err()
    );

    let members = members1_result.unwrap();
    assert_eq!(members.members.len(), 2, "DM channel should have exactly 2 members");

    let member_ids: Vec<&str> = members.members.iter().map(|m| m.user_id.as_str()).collect();
    assert!(
        member_ids.contains(&user1.user_id.as_str()),
        "User1 should be a member of the DM channel"
    );
    assert!(
        member_ids.contains(&user2.user_id.as_str()),
        "User2 should be a member of the DM channel"
    );

    // ========== Step 7: Verify display names are available ==========
    // The sidebar should show the OTHER user's name, not your own

    // Create fresh clients to avoid borrow issues
    let mut user1_profile_client = UserClient::new(pool1.channel(), pool1.auth_clone());
    let mut user2_profile_client = UserClient::new(pool2.channel(), pool2.auth_clone());

    // Get user profiles to verify display names are available
    let user1_gets_user2 = user1_profile_client.get_user_profile(&user2.user_id).await;
    assert!(
        user1_gets_user2.is_ok(),
        "User1 should be able to get User2's profile for sidebar display: {:?}",
        user1_gets_user2.err()
    );

    let profile2 = user1_gets_user2.unwrap();
    assert!(
        !profile2.display_name.is_empty(),
        "User2's display name should not be empty for sidebar"
    );

    let user2_gets_user1 = user2_profile_client.get_user_profile(&user1.user_id).await;
    assert!(
        user2_gets_user1.is_ok(),
        "User2 should be able to get User1's profile for sidebar display: {:?}",
        user2_gets_user1.err()
    );

    let profile1 = user2_gets_user1.unwrap();
    assert!(
        !profile1.display_name.is_empty(),
        "User1's display name should not be empty for sidebar"
    );

    // Log the expected sidebar display
    println!("=== DM Channel Sidebar Display ===");
    println!("Channel ID: {}", channel_id);
    println!("User1 ({}) should see: '{}'", user1.user_id, profile2.display_name);
    println!("User2 ({}) should see: '{}'", user2.user_id, profile1.display_name);
}

/// Test that the sender receives notification when their friend request is accepted.
///
/// This simulates what happens via inbox events when the other side accepts.
#[tokio::test]
async fn test_friend_accept_sender_gets_notification() {
    skip_if_no_server!();

    // Setup two users
    let pool1 = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool1).await);

    let pool2 = assert_ok(create_test_pool().await);
    let user2 = assert_ok(setup_authenticated_test(&pool2).await);

    use chatter_linux::network::grpc::UserClient;
    use chatter_linux::proto::chat_v1::FriendStatus;
    use tokio_stream::StreamExt;

    // User1 sends friend request
    let mut user1_client = UserClient::new(pool1.channel(), pool1.auth_clone());
    let _ = user1_client
        .send_friend_request(&user2.user_id, None)
        .await
        .expect("Friend request should succeed");

    // User2 accepts
    let mut user2_client = UserClient::new(pool2.channel(), pool2.auth_clone());
    let mut accept_stream = user2_client
        .respond_friend_request(&user1.user_id, FriendStatus::Accepted, None)
        .await
        .expect("Accept should succeed");

    // Drain accept stream to complete the operation
    let mut dm_channel_id = String::new();
    while let Some(event) = accept_stream.next().await {
        if let Ok(e) = event {
            if let Some(chatter_linux::proto::chat_v1::user_event::Event::FriendStatusChanged(status)) = e.event {
                dm_channel_id = status.channel_id;
            }
        }
    }
    // Drop stream to release borrow on user2_client
    drop(accept_stream);

    assert!(!dm_channel_id.is_empty(), "DM channel should be created");

    // User1 should be able to list friends and see User2
    let friends_result = user1_client.list_friends(10, 0).await;
    assert!(
        friends_result.is_ok(),
        "User1 should be able to list friends: {:?}",
        friends_result.err()
    );

    let friends = friends_result.unwrap();
    let friend_ids: Vec<&str> = friends.friends.iter().map(|f| f.user_id.as_str()).collect();

    assert!(
        friend_ids.contains(&user2.user_id.as_str()),
        "User2 should appear in User1's friend list after acceptance"
    );

    // Verify the friendship is bidirectional
    let friends2_result = user2_client.list_friends(10, 0).await;
    assert!(friends2_result.is_ok());

    let friends2 = friends2_result.unwrap();
    let friend_ids2: Vec<&str> = friends2.friends.iter().map(|f| f.user_id.as_str()).collect();

    assert!(
        friend_ids2.contains(&user1.user_id.as_str()),
        "User1 should appear in User2's friend list after acceptance"
    );

    println!("=== Friend Request Acceptance ===");
    println!("User1 ({}) friends: {:?}", user1.user_id, friend_ids);
    println!("User2 ({}) friends: {:?}", user2.user_id, friend_ids2);
    println!("DM Channel: {}", dm_channel_id);
}

/// Test that DM sidebar item data is correctly structured for display.
#[tokio::test]
async fn test_dm_sidebar_item_has_correct_structure() {
    skip_if_no_server!();

    // Setup two users and create friendship
    let pool1 = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool1).await);

    let pool2 = assert_ok(create_test_pool().await);
    let user2 = assert_ok(setup_authenticated_test(&pool2).await);

    use chatter_linux::network::grpc::UserClient;
    use chatter_linux::proto::chat_v1::FriendStatus;
    use tokio_stream::StreamExt;

    // Create friendship
    let mut user1_client = UserClient::new(pool1.channel(), pool1.auth_clone());
    let _ = user1_client.send_friend_request(&user2.user_id, None).await;

    let mut user2_client = UserClient::new(pool2.channel(), pool2.auth_clone());
    let mut accept_stream = user2_client
        .respond_friend_request(&user1.user_id, FriendStatus::Accepted, None)
        .await
        .expect("Accept should succeed");

    let mut dm_channel_id = String::new();
    while let Some(event) = accept_stream.next().await {
        if let Ok(e) = event {
            if let Some(chatter_linux::proto::chat_v1::user_event::Event::FriendStatusChanged(status)) = e.event {
                dm_channel_id = status.channel_id;
            }
        }
    }

    // Simulate what the UI callback would do to create DMListItemData
    // Create fresh clients to avoid borrow issues from stream consumption
    let mut user1_profile_client = UserClient::new(pool1.channel(), pool1.auth_clone());
    let mut user2_profile_client = UserClient::new(pool2.channel(), pool2.auth_clone());

    // For User1's sidebar, showing User2:
    let user2_profile = user1_profile_client.get_user_profile(&user2.user_id).await.unwrap();

    // Helper function to get initials (same as in inbox_events.rs)
    fn get_initials(name: &str) -> String {
        name.split_whitespace()
            .take(2)
            .filter_map(|w| w.chars().next())
            .collect::<String>()
            .to_uppercase()
    }

    // Create the sidebar item data structure
    let dm_item_for_user1 = (
        dm_channel_id.clone(),                    // channel_id - for navigation
        user2.user_id.clone(),                    // user_id - the OTHER user's ID
        user2_profile.display_name.clone(),       // display_name - show the OTHER user's name
        get_initials(&user2_profile.display_name), // initials
    );

    // For User2's sidebar, showing User1:
    let user1_profile = user2_profile_client.get_user_profile(&user1.user_id).await.unwrap();

    let dm_item_for_user2 = (
        dm_channel_id.clone(),                    // channel_id
        user1.user_id.clone(),                    // user_id
        user1_profile.display_name.clone(),       // display_name
        get_initials(&user1_profile.display_name), // initials
    );

    // Verify the structure is correct
    assert!(!dm_item_for_user1.0.is_empty(), "Channel ID should not be empty");
    assert!(!dm_item_for_user1.1.is_empty(), "User ID should not be empty");
    assert!(!dm_item_for_user1.2.is_empty(), "Display name should not be empty");
    assert!(!dm_item_for_user1.3.is_empty(), "Initials should not be empty");

    // Verify we're showing the OTHER user's name, not our own
    assert_ne!(
        dm_item_for_user1.2, user1.display_name,
        "User1's sidebar should show User2's name, not their own"
    );
    assert_ne!(
        dm_item_for_user2.2, user2.display_name,
        "User2's sidebar should show User1's name, not their own"
    );

    println!("=== DM Sidebar Item Structure ===");
    println!("User1's sidebar item: channel={}, other_user={}, display_name='{}', initials='{}'",
        dm_item_for_user1.0, dm_item_for_user1.1, dm_item_for_user1.2, dm_item_for_user1.3);
    println!("User2's sidebar item: channel={}, other_user={}, display_name='{}', initials='{}'",
        dm_item_for_user2.0, dm_item_for_user2.1, dm_item_for_user2.2, dm_item_for_user2.3);
}
