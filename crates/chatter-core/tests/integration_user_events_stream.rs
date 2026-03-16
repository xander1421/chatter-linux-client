//! Integration tests for user events stream.
//!
//! Tests that the server pushes real-time events through StreamUserEvents
//! when friend requests are sent, accepted, or declined.

mod common;

use std::time::Duration;

use chatter_linux::network::grpc::UserClient;
use chatter_linux::proto::chat_v1::{user_event::Event, FriendStatus};
use tokio_stream::StreamExt;

use common::*;

/// Test that User2 receives a FriendStatusChanged event when User1 sends them a friend request.
///
/// This verifies the server's StreamUserEvents RPC properly pushes events to subscribers.
#[tokio::test]
async fn test_friend_request_triggers_stream_event() {
    skip_if_no_server!();

    // Create two separate pools for two users (each with their own auth)
    let pool1 = assert_ok(create_streaming_test_pool().await);
    let pool2 = assert_ok(create_streaming_test_pool().await);

    let result = async {
        // Register and authenticate user1
        let user1 = assert_ok(setup_authenticated_test(&pool1).await);
        println!("User1 registered: {} ({})", user1.username, user1.user_id);

        // Register and authenticate user2
        let user2 = assert_ok(setup_authenticated_test(&pool2).await);
        println!("User2 registered: {} ({})", user2.username, user2.user_id);

        // Clone user2's ID to avoid lifetime issues with streams
        let user2_id = user2.user_id.clone();

        // User2 subscribes to their user events stream
        let mut user2_client = UserClient::new(pool2.channel(), pool2.auth_clone());
        let stream_result = user2_client.stream_user_events(&user2_id).await;
        assert!(stream_result.is_ok(), "User2 should be able to subscribe to user events stream");

        let mut user2_stream = stream_result.unwrap();
        println!("User2 subscribed to user events stream");

        // Consume initial "stream ready" event if any
        let _ = tokio::time::timeout(Duration::from_millis(500), user2_stream.next()).await;

        // User1 sends friend request to User2
        {
            let mut user1_client = UserClient::new(pool1.channel(), pool1.auth_clone());
            let send_result = user1_client
                .send_friend_request(&user2_id, Some("Hey, let's be friends!"))
                .await;

            assert!(send_result.is_ok(), "User1 should be able to send friend request to User2");
            println!("User1 sent friend request to User2");

            // Consume User1's own stream response
            let user1_send_stream = send_result.unwrap();
            tokio::pin!(user1_send_stream);
            while let Some(_) = user1_send_stream.next().await {}
        }

        // User2 should receive a FriendStatusChanged event
        let event_result = tokio::time::timeout(
            Duration::from_secs(10),
            async {
                while let Some(event_result) = user2_stream.next().await {
                    match event_result {
                        Ok(event) => {
                            println!("User2 received event: {:?}", event.event);
                            if let Some(Event::FriendStatusChanged(status)) = event.event {
                                println!(
                                    "FriendStatusChanged: user_id={}, status={:?}",
                                    status.user_id, status.status
                                );
                                return Some(status);
                            }
                        }
                        Err(e) => {
                            println!("Stream error: {}", e);
                            return None;
                        }
                    }
                }
                None
            }
        ).await;

        // Drop streams/clients explicitly
        drop(user2_stream);
        drop(user2_client);

        event_result
    }.await;

    // Cleanup - disconnect pools
    pool1.disconnect();
    pool2.disconnect();

    match result {
        Ok(Some(status)) => {
            println!("SUCCESS: User2 received friend request notification via stream!");
            assert!(!status.user_id.is_empty());
        }
        Ok(None) => {
            panic!("Stream ended without receiving FriendStatusChanged event");
        }
        Err(_) => {
            panic!(
                "TIMEOUT: User2 did not receive FriendStatusChanged event within 10 seconds.\n\
                This indicates the server is NOT pushing events through StreamUserEvents."
            );
        }
    }
}

/// Test that the stream receives events for friend request acceptance.
#[tokio::test]
async fn test_friend_accept_triggers_stream_event() {
    skip_if_no_server!();

    let pool1 = assert_ok(create_streaming_test_pool().await);
    let pool2 = assert_ok(create_streaming_test_pool().await);

    let result = async {
        let user1 = assert_ok(setup_authenticated_test(&pool1).await);
        let user2 = assert_ok(setup_authenticated_test(&pool2).await);
        println!("Users registered: {} and {}", user1.username, user2.username);

        // Clone IDs to avoid lifetime issues with streams
        let user1_id = user1.user_id.clone();
        let user2_id = user2.user_id.clone();

        // User1 sends friend request to User2
        {
            let mut user1_client = UserClient::new(pool1.channel(), pool1.auth_clone());
            let send_stream = assert_ok(
                user1_client.send_friend_request(&user2_id, None).await
            );
            tokio::pin!(send_stream);
            while let Some(_) = send_stream.next().await {}
        }
        println!("User1 sent friend request to User2");

        // User1 subscribes to their stream
        let mut user1_client = UserClient::new(pool1.channel(), pool1.auth_clone());
        let mut user1_stream = assert_ok(user1_client.stream_user_events(&user1_id).await);

        // Consume initial event
        let _ = tokio::time::timeout(Duration::from_millis(500), user1_stream.next()).await;

        // User2 accepts the friend request
        {
            let mut user2_client = UserClient::new(pool2.channel(), pool2.auth_clone());
            let accept_stream = assert_ok(
                user2_client.respond_friend_request(&user1_id, FriendStatus::Accepted, None).await
            );
            tokio::pin!(accept_stream);
            while let Some(_) = accept_stream.next().await {}
        }
        println!("User2 accepted friend request from User1");

        // User1 should receive acceptance event
        let event_result = tokio::time::timeout(
            Duration::from_secs(10),
            async {
                while let Some(event_result) = user1_stream.next().await {
                    if let Ok(event) = event_result {
                        if let Some(Event::FriendStatusChanged(status)) = event.event {
                            return Some(status);
                        }
                    }
                }
                None
            }
        ).await;

        drop(user1_stream);
        drop(user1_client);

        event_result
    }.await;

    // Cleanup
    pool1.disconnect();
    pool2.disconnect();

    match result {
        Ok(Some(status)) => {
            println!("SUCCESS: User1 received acceptance notification via stream!");
            if !status.channel_id.is_empty() {
                println!("DM channel created: {}", status.channel_id);
            }
        }
        Ok(None) => {
            panic!("Stream ended without receiving acceptance event");
        }
        Err(_) => {
            panic!("TIMEOUT: User1 did not receive friend acceptance event.");
        }
    }
}

/// Test that stream properly handles concurrent subscriptions.
#[tokio::test]
async fn test_multiple_stream_subscribers() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);

    {
        let user = assert_ok(setup_authenticated_test(&pool).await);
        let user_id = user.user_id.clone();

        let mut client1 = UserClient::new(pool.channel(), pool.auth_clone());
        let mut client2 = UserClient::new(pool.channel(), pool.auth_clone());

        let stream1 = client1.stream_user_events(&user_id).await;
        let stream2 = client2.stream_user_events(&user_id).await;

        assert!(stream1.is_ok(), "First stream subscription should succeed");
        assert!(stream2.is_ok(), "Second stream subscription should succeed");

        println!("SUCCESS: Multiple stream subscriptions work correctly");

        drop(stream1);
        drop(stream2);
        drop(client1);
        drop(client2);
    }

    pool.disconnect();
}

/// Test stream reconnection behavior.
#[tokio::test]
async fn test_stream_reconnection() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);

    {
        let user = assert_ok(setup_authenticated_test(&pool).await);
        let user_id = user.user_id.clone();
        let mut client = UserClient::new(pool.channel(), pool.auth_clone());

        // First subscription
        {
            let stream = assert_ok(client.stream_user_events(&user_id).await);
            let _ = tokio::time::timeout(Duration::from_millis(100), async {
                tokio::pin!(stream);
                stream.next().await
            }).await;
        }

        // Reconnect - should work
        let stream2 = client.stream_user_events(&user_id).await;
        assert!(stream2.is_ok(), "Stream reconnection should succeed");

        println!("SUCCESS: Stream reconnection works correctly");

        drop(stream2);
        drop(client);
    }

    pool.disconnect();
}
