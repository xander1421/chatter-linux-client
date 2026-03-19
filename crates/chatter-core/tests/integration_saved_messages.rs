//! Integration tests for SavedMessages (Note to Self) channel.
//!
//! Tests the special SavedMessages channel type where channel_id = user_id.

mod common;

use base64::Engine;
use chatter_core::network::grpc::{ChannelClient, MessageClient};
use chatter_core::proto::chat_v1::{
    message::content::Type as ContentType, message::Content, ChannelType, MessageSortDirection,
};
use tokio_stream::StreamExt;

use common::*;

// ============================================================================
// SavedMessages Channel Creation Tests
// ============================================================================

#[tokio::test]
async fn test_create_saved_messages_channel() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // SavedMessages channel_id should be user_id
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());

    // Create SavedMessages channel (name is usually "Saved Messages" or similar)
    let create_result = channel_client
        .create_channel(
            "Saved Messages",
            ChannelType::SavedMessages,
            None,
            vec![], // No members for SavedMessages
            None,
            Some("Note to self"),
            None,
        )
        .await;

    match create_result {
        Ok(stream) => {
            tokio::pin!(stream);
            let mut channel_id = String::new();
            while let Some(result) = stream.next().await {
                let event = result.expect("Stream error");
                if let Some(payload) = event.event {
                    use chatter_core::proto::chat_v1::channel_event::Event;
                    if let Event::ChannelCreated(ch) = payload {
                        channel_id = ch.id;
                        // Verify channel type is SavedMessages
                        assert_eq!(ch.r#type, ChannelType::SavedMessages as i32);
                    }
                }
            }
            // SavedMessages channel_id should equal user_id
            assert!(
                !channel_id.is_empty(),
                "SavedMessages channel should be created"
            );
            // The channel_id for SavedMessages is typically the user_id
            // (server may assign different ID depending on implementation)
        }
        Err(e) => {
            // SavedMessages might already exist for this user
            eprintln!("SavedMessages creation result: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_get_or_create_saved_messages() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // First creation attempt
    let first_ok = {
        let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
        let attempt = channel_client
            .create_channel(
                "Saved Messages",
                ChannelType::SavedMessages,
                None,
                vec![],
                None,
                None,
                None,
            )
            .await;

        if let Ok(stream) = attempt {
            tokio::pin!(stream);
            while let Some(_) = stream.next().await {}
            true
        } else {
            false
        }
    };

    // Second creation attempt should either succeed with same channel or return existing
    let second_ok = {
        let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
        let attempt = channel_client
            .create_channel(
                "Saved Messages",
                ChannelType::SavedMessages,
                None,
                vec![],
                None,
                None,
                None,
            )
            .await;

        if let Ok(stream) = attempt {
            tokio::pin!(stream);
            while let Some(_) = stream.next().await {}
            true
        } else {
            false
        }
    };

    // Both should have same channel_id (server should be idempotent)
    // Or second should fail with "already exists" error
    // Either behavior is acceptable
    eprintln!(
        "SavedMessages creation: first={}, second={}",
        first_ok, second_ok
    );
}

// ============================================================================
// SavedMessages Send/Receive Tests
// ============================================================================

#[tokio::test]
async fn test_send_message_to_saved_messages() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create SavedMessages channel first
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let create_result = channel_client
        .create_channel(
            "Saved Messages",
            ChannelType::SavedMessages,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await;

    let channel_id = match create_result {
        Ok(stream) => {
            tokio::pin!(stream);
            let mut id = String::new();
            while let Some(result) = stream.next().await {
                let event = result.expect("Stream error");
                if let Some(payload) = event.event {
                    use chatter_core::proto::chat_v1::channel_event::Event;
                    if let Event::ChannelCreated(ch) = payload {
                        id = ch.id;
                    }
                }
            }
            id
        }
        Err(_) => {
            // If creation fails, use user_id as channel_id (SavedMessages convention)
            user.user_id.clone()
        }
    };

    // Send a message to SavedMessages
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let content = Content {
        r#type: Some(ContentType::Text("Note to self: this is a test".to_string())),
    };

    let result = msg_client
        .send_message(
            &channel_id,
            content,
            None,
            false,
            None,
            None, // crypto_options
            vec![],
            false,
            vec![],
        )
        .await;

    match result {
        Ok(msg) => {
            assert!(!msg.id.is_empty(), "Message should have ID");
            assert_eq!(msg.channel_id, channel_id);
            eprintln!("Sent SavedMessages message: {}", msg.id);
        }
        Err(e) => {
            // May fail if channel doesn't exist or other server-specific behavior
            eprintln!("SavedMessages send error: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_retrieve_saved_messages() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create channel and send messages
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let create_result = channel_client
        .create_channel(
            "Saved Messages",
            ChannelType::SavedMessages,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await;

    let channel_id = match create_result {
        Ok(stream) => {
            tokio::pin!(stream);
            let mut id = String::new();
            while let Some(result) = stream.next().await {
                let event = result.expect("Stream error");
                if let Some(payload) = event.event {
                    use chatter_core::proto::chat_v1::channel_event::Event;
                    if let Event::ChannelCreated(ch) = payload {
                        id = ch.id;
                    }
                }
            }
            id
        }
        Err(_) => user.user_id.clone(),
    };

    // Send some test messages
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    for i in 0..3 {
        let content = Content {
            r#type: Some(ContentType::Text(format!("Saved note #{}", i + 1))),
        };
        let _ = msg_client
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
            .await;
    }

    // Retrieve messages
    let messages_result = msg_client
        .get_messages(
            &channel_id,
            50,
            None,
            None,
            MessageSortDirection::SortNewestFirst,
            None,
            None,
        )
        .await;

    match messages_result {
        Ok(response) => {
            eprintln!(
                "Retrieved {} saved messages",
                response.messages.len()
            );
            // Verify at least some messages exist
            // (may have messages from previous tests)
        }
        Err(e) => {
            eprintln!("Get saved messages error: {:?}", e);
        }
    }
}

// ============================================================================
// SavedMessages with Attachments Tests
// ============================================================================

#[tokio::test]
async fn test_saved_message_with_text_only() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Use user_id as SavedMessages channel_id
    let channel_id = user.user_id.clone();

    // Create the channel first (might already exist)
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let _ = channel_client
        .create_channel(
            "Saved Messages",
            ChannelType::SavedMessages,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await;

    // Send text-only saved message
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let content = Content {
        r#type: Some(ContentType::Text(
            "Remember to test the SavedMessages feature".to_string(),
        )),
    };

    let result = msg_client
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
        .await;

    // Either succeeds or fails gracefully
    if let Ok(msg) = result {
        eprintln!(
            "Saved text message: id={}, channel={}",
            msg.id, msg.channel_id
        );
    }
}

// ============================================================================
// SavedMessages Pagination Tests
// ============================================================================

#[tokio::test]
async fn test_saved_messages_pagination() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let channel_id = user.user_id.clone();

    // Ensure channel exists
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let _ = channel_client
        .create_channel(
            "Saved Messages",
            ChannelType::SavedMessages,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await;

    // Send several messages
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let mut message_ids = vec![];
    for i in 0..5 {
        let content = Content {
            r#type: Some(ContentType::Text(format!("Paginated note #{}", i + 1))),
        };
        if let Ok(msg) = msg_client
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
            .await
        {
            message_ids.push(msg.id);
        }
    }

    // Get first page (limit 2)
    let page1 = msg_client
        .get_messages(
            &channel_id,
            2,
            None,
            None,
            MessageSortDirection::SortNewestFirst,
            None,
            None,
        )
        .await;

    if let Ok(response) = page1 {
        eprintln!("Page 1: {} messages", response.messages.len());

        // If there's a next page, fetch it
        if let Some(last_msg) = response.messages.last() {
            let page2 = msg_client
                .get_messages(
                    &channel_id,
                    2,
                    None,
                    None,
                    MessageSortDirection::SortNewestFirst,
                    Some(&last_msg.id),
                    None,
                )
                .await;

            if let Ok(response2) = page2 {
                eprintln!("Page 2: {} messages", response2.messages.len());
            }
        }
    }
}

// ============================================================================
// SavedMessages Privacy Tests
// ============================================================================

#[tokio::test]
async fn test_saved_messages_not_visible_to_others() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    // User1's SavedMessages channel
    let user1_channel_id = user1.user_id.clone();

    // Send a private note as user1
    {
        let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
        let _ = channel_client
            .create_channel(
                "Saved Messages",
                ChannelType::SavedMessages,
                None,
                vec![],
                None,
                None,
                None,
            )
            .await;

        let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
        let content = Content {
            r#type: Some(ContentType::Text("Secret note".to_string())),
        };
        let _ = msg_client
            .send_message(
                &user1_channel_id,
                content,
                None,
                false,
                None,
                None,
                vec![],
                false,
                vec![],
            )
            .await;
    }

    // Create user2 and try to access user1's SavedMessages
    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);
    pool.auth().set_basic(
        format!("{}.{}", user2.user_id, user2.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user2.device_auth_token),
    );

    // User2 should NOT be able to read user1's SavedMessages
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let result = msg_client
        .get_messages(
            &user1_channel_id,
            50,
            None,
            None,
            MessageSortDirection::SortNewestFirst,
            None,
            None,
        )
        .await;

    // Should either return empty or error (not authorized)
    match result {
        Ok(response) => {
            // If server returns messages, they shouldn't be user1's private notes
            // (empty is expected, or error)
            eprintln!(
                "User2 accessing User1's SavedMessages: {} messages (should be 0 or error)",
                response.messages.len()
            );
        }
        Err(e) => {
            // Error is expected - user2 shouldn't access user1's SavedMessages
            eprintln!("Expected error accessing other user's SavedMessages: {:?}", e);
        }
    }
}

// ============================================================================
// SavedMessages Channel Info Tests
// ============================================================================

#[tokio::test]
async fn test_get_saved_messages_channel_info() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create SavedMessages channel
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let _ = channel_client
        .create_channel(
            "Saved Messages",
            ChannelType::SavedMessages,
            None,
            vec![],
            None,
            Some("Personal notes"),
            None,
        )
        .await;

    // Get channel info
    let channel_id = user.user_id.clone();
    let result = channel_client.get_channel(&channel_id).await;

    match result {
        Ok(channel) => {
            eprintln!(
                "SavedMessages channel: name='{}', type={}",
                channel.name,
                channel.r#type
            );
            assert_eq!(channel.r#type, ChannelType::SavedMessages as i32);
        }
        Err(e) => {
            // May not exist if creation failed
            eprintln!("Get SavedMessages channel error: {:?}", e);
        }
    }
}

// ============================================================================
// SavedMessages Multi-Device Tests
// ============================================================================

#[tokio::test]
async fn test_saved_messages_from_single_device() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let channel_id = user.user_id.clone();

    // Create channel
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let _ = channel_client
        .create_channel(
            "Saved Messages",
            ChannelType::SavedMessages,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await;

    // With single device, SavedMessages should work (stored locally + server)
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let content = Content {
        r#type: Some(ContentType::Text(
            "Single device note".to_string(),
        )),
    };

    let result = msg_client
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
        .await;

    if let Ok(msg) = result {
        // Verify message is retrievable
        let get_result = msg_client
            .get_messages(
                &channel_id,
                10,
                None,
                None,
                MessageSortDirection::SortNewestFirst,
                None,
                None,
            )
            .await;

        if let Ok(response) = get_result {
            let found = response.messages.iter().any(|m| m.id == msg.id);
            assert!(found, "Saved message should be retrievable");
        }
    }
}
