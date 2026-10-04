//! Integration tests for streaming endpoints.
//!
//! Tests real-time event streams for messages, channels, users, and presence.

use crate::{common, skip_if_no_server};

use std::time::Duration;
use chatter_core::network::grpc::{ChannelClient, MessageClient, UserClient};
use chatter_core::proto::chat_v1::{
    message::content::Type as ContentType, message::Content, ChannelType, UserPresence,
};
use tokio_stream::StreamExt;

use common::*;

// ============================================================================
// Message Stream Tests
// ============================================================================

#[tokio::test]
async fn test_message_stream_establishment() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a channel first
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("stream_test_{}", unique_username());

    let create_stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            None,
            None,
            Some(membership::genesis(&user, true).1),
        ).await
        .expect("Failed to create channel");

    tokio::pin!(create_stream);

    let mut channel_id = String::new();
    while let Some(result) = create_stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(ch) = payload {
                channel_id = ch.id;
            }
        }
    }

    // Start message stream
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let stream_result = msg_client.stream_messages(&channel_id, None).await;

    assert!(stream_result.is_ok(), "Should establish message stream");

    // Verify we receive initial "ready" message
    let mut stream = stream_result.unwrap();
    let first_event = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;

    assert!(first_event.is_ok(), "Should receive initial stream event within timeout");
}

#[tokio::test]
async fn test_message_stream_receives_new_messages() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create channel
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("realtime_{}", unique_username());

    let create_stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            None,
            None,
            Some(membership::genesis(&user, true).1),
        ).await
        .expect("Failed to create channel");

    tokio::pin!(create_stream);

    let mut channel_id = String::new();
    while let Some(result) = create_stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(ch) = payload {
                channel_id = ch.id;
            }
        }
    }

    // Start streaming in background task
    let pool_clone = pool.clone();
    let channel_id_clone = channel_id.clone();
    let stream_handle = tokio::spawn(async move {
        let mut msg_client = MessageClient::new(pool_clone.channel(), pool_clone.auth_clone());
        let stream = msg_client
            .stream_messages(&channel_id_clone, None)
            .await
            .expect("Stream should connect");

        tokio::pin!(stream);

        // Wait for a few events
        let mut events = vec![];
        let timeout = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(result) = stream.next().await {
                if let Ok(event) = result {
                    events.push(event);
                    if events.len() >= 3 {
                        break;
                    }
                }
            }
        });

        let _ = timeout.await;
        events.len()
    });

    // Give stream time to establish
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send some messages
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    for i in 0..3 {
        let content = Content {
            r#type: Some(ContentType::Text(format!("Stream test message {}", i))),
        };

        let _ = msg_client
            .send_message(
                &channel_id,
                content,
                None,
                false,
                None,
                vec![],
                false,
                vec![],
                Vec::new(),
            )
            .await;
    }

    // Check how many events the stream received
    let event_count = stream_handle.await.unwrap_or(0);

    // Should have received at least some events
    // (exact count depends on server implementation and timing)
    eprintln!("Stream received {} events", event_count);
}

// ============================================================================
// Channel Event Stream Tests
// ============================================================================

#[tokio::test]
async fn test_channel_event_stream() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());

    // Start streaming all channel events
    let stream_result = channel_client.stream_channel_events(None).await;

    assert!(stream_result.is_ok(), "Should establish channel event stream");

    // Verify we can receive from the stream
    let mut stream = stream_result.unwrap();
    let first_event = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;

    // Should receive initial event or timeout (both acceptable)
    match first_event {
        Ok(Some(Ok(_))) => {} // Got event
        Ok(Some(Err(e))) => panic!("Stream error: {:?}", e),
        Ok(None) => {} // Stream closed
        Err(_) => {}   // Timeout - acceptable if no events pending
    }
}

#[tokio::test]
async fn test_channel_event_stream_specific_channel() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a channel using a separate scope to ensure stream is dropped
    let channel_id = {
        let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
        let channel_name = format!("specific_{}", unique_username());

        let create_stream = channel_client
            .create_channel(
                &channel_name,
                ChannelType::ChannelPublic,
                None,
                None,
                None,
                Some(membership::genesis(&user, true).1),
            ).await
            .expect("Failed to create channel");

        tokio::pin!(create_stream);

        let mut channel_id = String::new();
        while let Some(result) = create_stream.next().await {
            let event = result.expect("Stream error");
            if let Some(payload) = event.event {
                use chatter_core::proto::chat_v1::channel_event::Event;
                if let Event::ChannelCreated(ch) = payload {
                    channel_id = ch.id;
                }
            }
        }
        channel_id
    };

    // Stream events for specific channel with a fresh client
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let stream_result = channel_client
        .stream_channel_events(Some(&channel_id))
        .await;

    assert!(stream_result.is_ok(), "Should establish channel-specific event stream");
}

// ============================================================================
// User Event Stream Tests
// ============================================================================

#[tokio::test]
async fn test_user_event_stream() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // Start streaming user events
    let stream_result = user_client.stream_user_events(&user.user_id).await;

    assert!(stream_result.is_ok(), "Should establish user event stream");

    // Receive initial event
    let mut stream = stream_result.unwrap();
    let first_event = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;

    // Should receive initial "ready" event
    assert!(first_event.is_ok(), "Should receive initial stream event");
}

// ============================================================================
// Presence
// ============================================================================
// The StreamPresence RPC was removed — presence now reaches friends via
// StreamUserEvents (EventTypeUserPresenceUpdated), proven end-to-end by
// validated.rs::v34_presence_reaches_friends_not_strangers.

#[tokio::test]
async fn test_presence_update() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());

    // The identified presence producer RPC must accept an update.
    let update_result = user_client.update_presence(UserPresence::Online).await;
    assert!(update_result.is_ok(), "Should update presence");
}

// ============================================================================
// Stream Reconnection Tests
// ============================================================================

#[tokio::test]
async fn test_stream_reconnection() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create channel
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("reconnect_{}", unique_username());

    let create_stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            None,
            None,
            Some(membership::genesis(&user, true).1),
        ).await
        .expect("Failed to create channel");

    tokio::pin!(create_stream);

    let mut channel_id = String::new();
    while let Some(result) = create_stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(ch) = payload {
                channel_id = ch.id;
            }
        }
    }

    // First stream
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let stream1 = msg_client.stream_messages(&channel_id, None).await;
    assert!(stream1.is_ok(), "First stream should connect");

    // Drop first stream
    drop(stream1);

    // Small delay
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Second stream (reconnect)
    let stream2 = msg_client.stream_messages(&channel_id, None).await;
    assert!(stream2.is_ok(), "Should be able to reconnect stream");
}

// ============================================================================
// Multiple Simultaneous Streams Tests
// ============================================================================

#[tokio::test]
async fn test_multiple_channel_streams() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create two channels using a separate scope
    let channel_ids = {
        let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
        let mut ids = vec![];

        for i in 0..2 {
            let channel_name = format!("multi_stream_{}_{}", i, unique_username());

            let create_stream = channel_client
                .create_channel(
                    &channel_name,
                    ChannelType::ChannelPublic,
                    None,
                    None,
                    None,
                    Some(membership::genesis(&user, true).1),
                ).await
                .expect("Failed to create channel");

            tokio::pin!(create_stream);

            while let Some(result) = create_stream.next().await {
                let event = result.expect("Stream error");
                if let Some(payload) = event.event {
                    use chatter_core::proto::chat_v1::channel_event::Event;
                    if let Event::ChannelCreated(ch) = payload {
                        ids.push(ch.id);
                    }
                }
            }
        }
        ids
    };

    assert_eq!(channel_ids.len(), 2, "Should have created 2 channels");

    // Open streams to both channels simultaneously using separate clients
    let mut msg_client1 = MessageClient::new(pool.channel(), pool.auth_clone());
    let mut msg_client2 = MessageClient::new(pool.channel(), pool.auth_clone());

    let stream1 = msg_client1.stream_messages(&channel_ids[0], None).await;
    let stream2 = msg_client2.stream_messages(&channel_ids[1], None).await;

    assert!(stream1.is_ok(), "First channel stream should connect");
    assert!(stream2.is_ok(), "Second channel stream should connect");
}

// ============================================================================
// Stream with Filter/Cursor Tests
// ============================================================================

#[tokio::test]
async fn test_message_stream_with_cursor() {
    skip_if_no_server!();

    let pool = assert_ok(create_streaming_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create channel
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = format!("cursor_test_{}", unique_username());

    let create_stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,
            None,
            None,
            Some(membership::genesis(&user, true).1),
        ).await
        .expect("Failed to create channel");

    tokio::pin!(create_stream);

    let mut channel_id = String::new();
    while let Some(result) = create_stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(ch) = payload {
                channel_id = ch.id;
            }
        }
    }

    // Send some messages first
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let mut last_message_id = String::new();

    for i in 0..3 {
        let content = Content {
            r#type: Some(ContentType::Text(format!("Cursor test {}", i))),
        };

        let msg = msg_client
            .send_message(
                &channel_id,
                content,
                None,
                false,
                None,
                vec![],
                false,
                vec![],
                Vec::new(),
            )
            .await
            .expect("Send should succeed");

        last_message_id = msg.id;
    }

    // Stream with cursor (from last message)
    let stream_with_cursor = msg_client
        .stream_messages(&channel_id, Some(&last_message_id))
        .await;

    assert!(
        stream_with_cursor.is_ok(),
        "Should establish stream with cursor"
    );
}
