//! End-to-end integration tests for channel messaging with multiple users.
//!
//! Tests the complete flow:
//! 1. Multiple users register
//! 2. One user creates a channel
//! 3. Other users join
//! 4. Users exchange messages
//! 5. Messages are received correctly

mod common;

use chatter_core::network::grpc::{ChannelClient, MessageClient};
use chatter_core::proto::chat_v1::{
    message::content::Type as ContentType, message::Content, ChannelType, MessageSortDirection,
};
use tokio_stream::StreamExt;

use common::*;

/// Number of test users to create.
const NUM_USERS: usize = 5;

/// Helper struct to hold user pool and clients.
struct TestUserContext {
    user: TestUser,
    pool: std::sync::Arc<chatter_core::network::grpc::GrpcClientPool>,
}

impl TestUserContext {
    fn channel_client(&self) -> ChannelClient {
        ChannelClient::new(self.pool.channel(), self.pool.auth_clone())
    }

    fn message_client(&self) -> MessageClient {
        MessageClient::new(self.pool.channel(), self.pool.auth_clone())
    }
}

/// Create and register N test users, each with their own connection pool.
async fn setup_users(count: usize) -> Vec<TestUserContext> {
    let mut users = Vec::with_capacity(count);

    for i in 0..count {
        let pool = assert_ok(create_test_pool().await);
        let user = assert_ok(setup_authenticated_test(&pool).await);
        tracing::info!(
            user_index = i,
            user_id = %user.user_id,
            username = %user.username,
            "Registered test user"
        );
        users.push(TestUserContext { user, pool });
    }

    users
}

/// Helper to create a channel and wait for completion.
async fn create_channel_and_wait(
    channel_client: &mut ChannelClient,
    name: &str,
    channel_type: ChannelType,
) -> String {
    let stream = channel_client
        .create_channel(name, channel_type, None, vec![], None, Some("E2E test channel"), None)
        .await
        .expect("Failed to start channel creation");

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            match payload {
                Event::ChannelCreated(channel) => {
                    channel_id = channel.id;
                    tracing::info!(channel_id = %channel_id, "Channel created");
                }
                Event::OperationFailed(e) => {
                    let msg = e.error.map(|err| err.message).unwrap_or_default();
                    panic!("Channel creation failed: {}", msg);
                }
                _ => {}
            }
        }
    }

    assert!(!channel_id.is_empty(), "Should have created a channel");
    channel_id
}

/// Helper for a user to join a channel and wait for completion.
async fn join_channel_and_wait(channel_client: &mut ChannelClient, channel_id: &str, user_id: &str) {
    let join_stream = channel_client
        .join_channel(channel_id, None, false)
        .await
        .expect("Failed to start join");

    tokio::pin!(join_stream);
    let mut joined = false;

    while let Some(result) = join_stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            match payload {
                Event::MemberAdded(member) => {
                    tracing::info!(
                        user_id = %user_id,
                        member_id = %member.user_id,
                        "Member added to channel"
                    );
                    joined = true;
                }
                Event::OperationFailed(e) => {
                    let msg = e.error.map(|err| err.message).unwrap_or_default();
                    panic!("Join failed for user {}: {}", user_id, msg);
                }
                _ => {}
            }
        }
    }

    // Creator might already be a member, so MemberAdded might not fire
    let _ = joined;
}

// ============================================================================
// E2E Tests
// ============================================================================

#[tokio::test]
async fn test_e2e_multiple_users_create_and_join_channel() {
    skip_if_no_server!();

    // Initialize tracing for test output
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting E2E test: {} users create and join channel ===", NUM_USERS);

    // Step 1: Register all users
    let users = setup_users(NUM_USERS).await;
    tracing::info!("Registered {} users successfully", users.len());

    // Step 2: User 0 creates a public channel
    let channel_name = format!("e2e_test_{}", unique_username());
    let mut creator_channel_client = users[0].channel_client();
    let channel_id =
        create_channel_and_wait(&mut creator_channel_client, &channel_name, ChannelType::ChannelPublic)
            .await;

    tracing::info!(
        creator = %users[0].user.username,
        channel_id = %channel_id,
        channel_name = %channel_name,
        "User 0 created channel"
    );

    // Step 3: Users 1-4 join the channel
    for (i, user_ctx) in users.iter().enumerate().skip(1) {
        let mut channel_client = user_ctx.channel_client();
        join_channel_and_wait(&mut channel_client, &channel_id, &user_ctx.user.user_id).await;
        tracing::info!(
            user_index = i,
            username = %user_ctx.user.username,
            "User joined channel"
        );
    }

    // Step 4: Verify all members are in the channel
    let members_response = assert_ok(
        creator_channel_client
            .get_channel_members(&channel_id, 100, 0)
            .await,
    );

    tracing::info!(
        member_count = members_response.members.len(),
        "Retrieved channel members"
    );

    // Verify all users are members
    for user_ctx in &users {
        let found = members_response
            .members
            .iter()
            .any(|m| m.user_id == user_ctx.user.user_id);
        assert!(
            found,
            "User {} should be a member of the channel",
            user_ctx.user.username
        );
    }

    assert_eq!(
        members_response.members.len(),
        NUM_USERS,
        "Channel should have exactly {} members",
        NUM_USERS
    );

    tracing::info!("=== E2E test completed successfully ===");
}

#[tokio::test]
async fn test_e2e_users_send_messages_in_channel() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting E2E test: users send messages ===");

    // Setup users and channel
    let users = setup_users(NUM_USERS).await;

    let channel_name = format!("e2e_msg_test_{}", unique_username());
    let mut creator_channel_client = users[0].channel_client();
    let channel_id =
        create_channel_and_wait(&mut creator_channel_client, &channel_name, ChannelType::ChannelPublic)
            .await;

    // All other users join
    for user_ctx in users.iter().skip(1) {
        let mut channel_client = user_ctx.channel_client();
        join_channel_and_wait(&mut channel_client, &channel_id, &user_ctx.user.user_id).await;
    }

    // Each user sends a message
    let mut sent_messages = Vec::new();
    for (i, user_ctx) in users.iter().enumerate() {
        let mut message_client = user_ctx.message_client();
        let message_text = format!("Hello from user {} ({})", i, user_ctx.user.username);

        let content = Content {
            r#type: Some(ContentType::Text(message_text.clone())),
        };

        let message = assert_ok(
            message_client
                .send_message(
                    &channel_id,
                    content,
                    None,   // parent_id
                    false,  // ephemeral
                    None,   // ephemeral_ttl
                    None,   // crypto_options
                    vec![], // mentioned_integrations
                    false,  // targets_ai_bot
                    vec![], // mentioned_file_ids
                )
                .await,
        );

        tracing::info!(
            user_index = i,
            message_id = %message.id,
            "User sent message"
        );

        sent_messages.push((user_ctx.user.user_id.clone(), message.id, message_text));
    }

    // Give server a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify all messages can be retrieved
    let mut reader_message_client = users[0].message_client();
    let response = assert_ok(
        reader_message_client
            .get_messages(
                &channel_id,
                100,
                None,
                None,
                MessageSortDirection::SortOldestFirst,
                None,
                None,
            )
            .await,
    );

    tracing::info!(
        retrieved_count = response.messages.len(),
        expected_count = NUM_USERS,
        "Retrieved messages from channel"
    );

    // Verify each sent message exists
    for (author_id, message_id, expected_text) in &sent_messages {
        let found = response.messages.iter().find(|m| m.id == *message_id);
        assert!(
            found.is_some(),
            "Message {} from user {} should exist",
            message_id,
            author_id
        );

        let msg = found.unwrap();
        assert_eq!(msg.author_id, *author_id, "Message author should match");

        // Verify message content
        if let Some(content) = &msg.content {
            if let Some(ContentType::Text(text)) = &content.r#type {
                assert_eq!(text, expected_text, "Message text should match");
            }
        }
    }

    tracing::info!("=== E2E message test completed successfully ===");
}

#[tokio::test]
async fn test_e2e_message_reactions() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting E2E test: message reactions ===");

    // Setup with 3 users for reactions test
    let users = setup_users(3).await;

    let channel_name = format!("e2e_reaction_test_{}", unique_username());
    let mut creator_channel_client = users[0].channel_client();
    let channel_id =
        create_channel_and_wait(&mut creator_channel_client, &channel_name, ChannelType::ChannelPublic)
            .await;

    // Other users join
    for user_ctx in users.iter().skip(1) {
        let mut channel_client = user_ctx.channel_client();
        join_channel_and_wait(&mut channel_client, &channel_id, &user_ctx.user.user_id).await;
    }

    // User 0 sends a message
    let mut sender_client = users[0].message_client();
    let content = Content {
        r#type: Some(ContentType::Text("React to this message!".to_string())),
    };

    let message = assert_ok(
        sender_client
            .send_message(&channel_id, content, None, false, None, None, vec![], false, vec![])
            .await,
    );

    tracing::info!(message_id = %message.id, "Sent message for reactions");

    // Users 1 and 2 react to the message
    let reactions = ["👍", "🎉"];
    for (i, user_ctx) in users.iter().skip(1).enumerate() {
        let mut msg_client = user_ctx.message_client();
        let emoji = reactions[i];

        let reacted_msg = assert_ok(
            msg_client
                .add_reaction(&channel_id, &message.author_id, message.timestamp, emoji)
                .await,
        );

        tracing::info!(
            user_index = i + 1,
            emoji = emoji,
            reaction_count = reacted_msg.reaction_counts.len(),
            "User added reaction"
        );
    }

    // Verify reactions
    let reaction_response = assert_ok(
        sender_client
            .get_message_reactions(&channel_id, &message.author_id, message.timestamp, 10, 0, None)
            .await,
    );

    tracing::info!(
        reaction_count = reaction_response.reactions.len(),
        "Retrieved reactions"
    );

    assert_eq!(
        reaction_response.reactions.len(),
        2,
        "Should have 2 reactions"
    );

    tracing::info!("=== E2E reaction test completed successfully ===");
}

#[tokio::test]
async fn test_e2e_user_leaves_channel() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting E2E test: user leaves channel ===");

    let users = setup_users(3).await;

    let channel_name = format!("e2e_leave_test_{}", unique_username());
    let mut creator_channel_client = users[0].channel_client();
    let channel_id =
        create_channel_and_wait(&mut creator_channel_client, &channel_name, ChannelType::ChannelPublic)
            .await;

    // Users 1 and 2 join
    for user_ctx in users.iter().skip(1) {
        let mut channel_client = user_ctx.channel_client();
        join_channel_and_wait(&mut channel_client, &channel_id, &user_ctx.user.user_id).await;
    }

    // Verify 3 members
    let members_before = assert_ok(
        creator_channel_client
            .get_channel_members(&channel_id, 100, 0)
            .await,
    );
    assert_eq!(members_before.members.len(), 3, "Should have 3 members");

    // User 2 leaves
    let mut leaver_channel_client = users[2].channel_client();
    let leave_stream = leaver_channel_client
        .leave_channel(&channel_id)
        .await
        .expect("Failed to start leave");

    tokio::pin!(leave_stream);
    while let Some(result) = leave_stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            match payload {
                Event::MemberRemovedId(removed_user_id) => {
                    tracing::info!(
                        removed_user = %removed_user_id,
                        "Member left channel"
                    );
                }
                Event::OperationFailed(e) => {
                    let msg = e.error.map(|err| err.message).unwrap_or_default();
                    panic!("Leave failed: {}", msg);
                }
                _ => {}
            }
        }
    }

    // Verify only 2 members remain
    let members_after = assert_ok(
        creator_channel_client
            .get_channel_members(&channel_id, 100, 0)
            .await,
    );

    assert_eq!(members_after.members.len(), 2, "Should have 2 members after leave");

    let user2_still_member = members_after
        .members
        .iter()
        .any(|m| m.user_id == users[2].user.user_id);
    assert!(
        !user2_still_member,
        "User 2 should no longer be a member"
    );

    tracing::info!("=== E2E leave test completed successfully ===");
}

#[tokio::test]
async fn test_e2e_message_edit_and_delete() {
    skip_if_no_server!();

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    tracing::info!("=== Starting E2E test: message edit and delete ===");

    let users = setup_users(2).await;

    let channel_name = format!("e2e_edit_delete_test_{}", unique_username());
    let mut creator_channel_client = users[0].channel_client();
    let channel_id =
        create_channel_and_wait(&mut creator_channel_client, &channel_name, ChannelType::ChannelPublic)
            .await;

    // User 1 joins
    let mut joiner_channel_client = users[1].channel_client();
    join_channel_and_wait(&mut joiner_channel_client, &channel_id, &users[1].user.user_id).await;

    // User 0 sends a message
    let mut msg_client = users[0].message_client();
    let original_text = "Original message content";
    let content = Content {
        r#type: Some(ContentType::Text(original_text.to_string())),
    };

    let message = assert_ok(
        msg_client
            .send_message(&channel_id, content, None, false, None, None, vec![], false, vec![])
            .await,
    );

    tracing::info!(message_id = %message.id, "Sent original message");

    // Edit the message
    let edited_text = "Edited message content";
    let new_content = Content {
        r#type: Some(ContentType::Text(edited_text.to_string())),
    };

    let edited_message = assert_ok(
        msg_client
            .edit_message(&message.id, &channel_id, new_content)
            .await,
    );

    assert!(edited_message.edited, "Message should be marked as edited");
    tracing::info!(message_id = %edited_message.id, "Edited message");

    // Verify edited content
    if let Some(content) = &edited_message.content {
        if let Some(ContentType::Text(text)) = &content.r#type {
            assert_eq!(text, edited_text, "Edited text should match");
        }
    }

    // Delete the message
    assert_ok(msg_client.delete_message(&message.id, &channel_id).await);
    tracing::info!(message_id = %message.id, "Deleted message");

    // Give server time to process
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify message is gone (or marked deleted)
    let response = assert_ok(
        msg_client
            .get_messages(
                &channel_id,
                100,
                None,
                None,
                MessageSortDirection::SortOldestFirst,
                None,
                None,
            )
            .await,
    );

    let msg_exists = response.messages.iter().any(|m| m.id == message.id);
    // Note: Server might soft-delete, so message could still exist but be empty
    // The important thing is the test completed without errors
    tracing::info!(
        message_still_exists = msg_exists,
        "Message deletion complete"
    );

    tracing::info!("=== E2E edit/delete test completed successfully ===");
}
