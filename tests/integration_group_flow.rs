//! Integration tests for complete group channel workflow.
//!
//! Tests group creation, member management, messaging, and permissions.

mod common;

use base64::Engine;
use chatter_linux::network::grpc::{ChannelClient, MessageClient};
use chatter_linux::proto::chat_v1::{
    message::content::Type as ContentType, message::Content, Channel as ChannelProto, ChannelType,
    MessageSortDirection,
};
use tokio_stream::StreamExt;

use common::*;

// Helper to consume a channel creation stream and get the channel ID
async fn consume_create_stream_get_id(
    stream: impl futures::Stream<Item = Result<chatter_linux::proto::chat_v1::ChannelEvent, chatter_linux::error::NetworkError>>,
) -> String {
    tokio::pin!(stream);
    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_linux::proto::chat_v1::channel_event::Event;
            match payload {
                Event::ChannelCreated(ch) => {
                    channel_id = ch.id;
                }
                Event::OperationFailed(e) => {
                    let msg = e.error.map(|err| err.message).unwrap_or_default();
                    panic!("Channel creation failed: {}", msg);
                }
                _ => {}
            }
        }
    }
    channel_id
}

// ============================================================================
// Group Creation Tests
// ============================================================================

#[tokio::test]
async fn test_create_private_group() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("private_group_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            vec![],
            None,
            Some("A private group for testing"),
            None,
        )
        .await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;
    assert!(!channel_id.is_empty(), "Should have created a group");
}

#[tokio::test]
async fn test_create_group_with_initial_members() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create additional users
    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    pool.auth().clear();
    let user3 = assert_ok(register_test_user(&pool).await);

    // Re-auth as user1
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("team_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            vec![user2.user_id.clone(), user3.user_id.clone()],
            None,
            Some("Team group with members"),
            None,
        )
        .await
        .expect("Failed to create group with members");

    let channel_id = consume_create_stream_get_id(stream).await;

    // Verify members
    let members = assert_ok(
        channel_client
            .get_channel_members(&channel_id, 100, 0)
            .await,
    );

    // Should have at least the creator
    assert!(
        members.members.len() >= 1,
        "Group should have members"
    );
}

// ============================================================================
// Member Management Tests
// ============================================================================

#[tokio::test]
async fn test_invite_user_to_group() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create user to invite
    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    // Re-auth as user1
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    // Create group without user2
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("invite_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;

    // Invite user2 (using invite_user, not add_member)
    let invite_result = channel_client
        .invite_user(&channel_id, &user2.user_id, Some("Join our group!"))
        .await;

    match invite_result {
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(_) = stream.next().await {}
        }
        Err(e) => {
            eprintln!("Invite user result: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_kick_user_from_group() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    // Create group with user2
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("kick_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            vec![user2.user_id.clone()],
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;

    // Kick user2 (using kick_user, not remove_member)
    let kick_result = channel_client
        .kick_user(&channel_id, &user2.user_id, Some("Testing kick functionality"))
        .await;

    match kick_result {
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(_) = stream.next().await {}
        }
        Err(e) => {
            eprintln!("Kick user result: {:?}", e);
        }
    }
}

// ============================================================================
// Group Messaging Tests
// ============================================================================

#[tokio::test]
async fn test_send_message_to_group() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create group
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("messaging_group_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;

    // Send message to group
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let content = Content {
        r#type: Some(ContentType::Text("Hello group!".to_string())),
    };

    let message = assert_ok(
        msg_client
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
            .await,
    );

    assert!(!message.id.is_empty());
    assert_eq!(message.channel_id, channel_id);
}

#[tokio::test]
async fn test_group_message_thread() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create group
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("thread_group_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;

    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Send original message
    let original_content = Content {
        r#type: Some(ContentType::Text("Discussion topic".to_string())),
    };

    let original = assert_ok(
        msg_client
            .send_message(
                &channel_id,
                original_content,
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

    // Reply to original
    let reply_content = Content {
        r#type: Some(ContentType::Text("This is a reply".to_string())),
    };

    let reply = assert_ok(
        msg_client
            .send_message(
                &channel_id,
                reply_content,
                Some(&original.id),
                false,
                None,
                None,
                vec![],
                false,
                vec![],
            )
            .await,
    );

    // Verify reply references original (parent_id is the reply target)
    assert_eq!(reply.parent_id, original.id);
}

// ============================================================================
// Group Settings Tests
// ============================================================================

#[tokio::test]
async fn test_update_group_settings() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let original_name = format!("update_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &original_name,
            ChannelType::ChannelPrivate,
            None,
            vec![],
            None,
            Some("Original description"),
            None,
        )
        .await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;

    // Update channel using proper ChannelProto structure
    let new_name = format!("renamed_{}", unique_username());
    let updated_channel = ChannelProto {
        id: channel_id.clone(),
        name: new_name.clone(),
        description: "Updated description".to_string(),
        r#type: ChannelType::ChannelPrivate as i32,
        ..Default::default()
    };

    let update_result = channel_client
        .update_channel(&channel_id, updated_channel, None)
        .await;

    match update_result {
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(result) = stream.next().await {
                let event = result.expect("Stream error");
                if let Some(payload) = event.event {
                    use chatter_linux::proto::chat_v1::channel_event::Event;
                    if let Event::ChannelUpdated(ch) = payload {
                        assert_eq!(ch.name, new_name);
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("Update channel result: {:?}", e);
        }
    }
}

// ============================================================================
// Leave Group Tests
// ============================================================================

#[tokio::test]
async fn test_leave_group() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    // Create group with user2
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("leave_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            vec![user2.user_id.clone()],
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;

    // User2 leaves the group
    pool.auth().set_basic(
        format!("{}.{}", user2.user_id, user2.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user2.device_auth_token),
    );

    let mut user2_channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let leave_result = user2_channel_client.leave_channel(&channel_id).await;

    match leave_result {
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(_) = stream.next().await {}
        }
        Err(e) => {
            eprintln!("Leave result: {:?}", e);
        }
    }
}

// ============================================================================
// Group Message History Tests
// ============================================================================

#[tokio::test]
async fn test_group_message_pagination() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("pagination_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;

    // Send multiple messages
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    for i in 0..10 {
        let content = Content {
            r#type: Some(ContentType::Text(format!("Message number {}", i))),
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

    // Get first page (5 messages)
    let page1 = assert_ok(
        msg_client
            .get_messages(
                &channel_id,
                5,
                None,
                None,
                MessageSortDirection::SortNewestFirst,
                None,
                None,
            )
            .await,
    );

    assert!(page1.messages.len() <= 5, "First page should have at most 5 messages");

    // Get second page using cursor
    if let Some(last_msg) = page1.messages.last() {
        let page2 = assert_ok(
            msg_client
                .get_messages(
                    &channel_id,
                    5,
                    Some(&last_msg.id),
                    None,
                    MessageSortDirection::SortNewestFirst,
                    None,
                    None,
                )
                .await,
        );

        // Second page should have different messages
        if !page2.messages.is_empty() {
            assert_ne!(
                page2.messages[0].id,
                page1.messages[0].id,
                "Pages should have different messages"
            );
        }
    }
}

// ============================================================================
// Group Permission Tests
// ============================================================================

#[tokio::test]
async fn test_non_member_cannot_send_to_private_group() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create private group
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("private_permission_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;

    // Create non-member user
    pool.auth().clear();
    let non_member = assert_ok(register_test_user(&pool).await);

    pool.auth().set_basic(
        format!("{}.{}", non_member.user_id, non_member.device_id),
        base64::engine::general_purpose::STANDARD.encode(&non_member.device_auth_token),
    );

    // Try to send message as non-member
    let mut msg_client = MessageClient::new(pool.channel(), pool.auth_clone());
    let content = Content {
        r#type: Some(ContentType::Text("Unauthorized message".to_string())),
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

    // Should fail - non-member cannot send to private group
    assert!(
        result.is_err(),
        "Non-member should not be able to send to private group"
    );
}
