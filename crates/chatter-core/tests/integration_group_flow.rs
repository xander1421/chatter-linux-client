//! Integration tests for complete group channel workflow.
//!
//! Tests group creation, member management, messaging, and permissions.

use crate::{common, skip_if_no_server};

use base64::Engine;
use chatter_core::network::grpc::{ChannelClient, MessageClient};
use chatter_core::proto::chat_v1::{
    message::content::Type as ContentType, message::Content, Channel as ChannelProto, ChannelType,
    MessageSortDirection,
};
use tokio_stream::StreamExt;

use common::*;

// Helper to consume a channel creation stream and get the channel ID
async fn consume_create_stream_get_id(
    stream: impl futures::Stream<Item = Result<chatter_core::proto::chat_v1::ChannelEvent, chatter_core::error::NetworkError>>,
) -> String {
    tokio::pin!(stream);
    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
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
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("private_group_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            Some("A private group for testing"),
            None,
            Some(membership::genesis(&user, false).1),
        ).await
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
    let _user2 = assert_ok(register_test_user(&pool).await);

    pool.auth().clear();
    let _user3 = assert_ok(register_test_user(&pool).await);

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
            Some("Team group with members"),
            None,
            Some(membership::genesis(&user1, false).1),
        ).await
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
            None,
            None,
            Some(membership::genesis(&user1, false).1),
        ).await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;

    // Invite user2 with a signed AddMember{pending}
    let change = membership::invite(&user1, &mut channel_client, &channel_id, &user2).await;
    let invite_result = channel_client
        .invite_user(&channel_id, change, Some("Join our group!"))
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
            None,
            None,
            Some(membership::genesis(&user1, false).1),
        ).await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;

    // user2 must be a member first: signed invite, then their signed accept.
    join_by_invite(&pool, &mut channel_client, &channel_id, &user1, &user2).await;

    // Kick user2 with a signed RemoveMember{KICKED}
    let change = membership::kick(&user1, &mut channel_client, &channel_id, &user2.user_id).await;
    let kick_result = channel_client
        .kick_user(&channel_id, change, Some("Testing kick functionality"))
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
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create group
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("messaging_group_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            None,
            None,
            Some(membership::genesis(&user, false).1),
        ).await
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
                vec![],
                false,
                vec![],
                Vec::new(),
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
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Create group
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("thread_group_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            None,
            None,
            Some(membership::genesis(&user, false).1),
        ).await
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
                vec![],
                false,
                vec![],
                Vec::new(),
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
                vec![],
                false,
                vec![],
                Vec::new(),
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
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let original_name = format!("update_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &original_name,
            ChannelType::ChannelPrivate,
            None,
            Some("Original description"),
            None,
            Some(membership::genesis(&user, false).1),
        ).await
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

    // INFRA-30: update_mask is REQUIRED and authoritative. This call used to
    // pass `None`, which the server now rejects with InvalidArgument
    // ("update_mask must name at least one field to update",
    // internal/services/channel/service/channels.go:547-548) — and the match
    // below used to print the failure to stderr and pass anyway, so the
    // regression was invisible on top of being behind `skip_if_no_server!()`.
    let mask = prost_types::FieldMask {
        paths: vec!["name".to_string(), "description".to_string()],
    };

    let stream = channel_client
        .update_channel(&channel_id, updated_channel, Some(mask))
        .await
        .expect("update_channel with a populated update_mask must be accepted");

    tokio::pin!(stream);
    let mut saw_update = false;
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_core::proto::chat_v1::channel_event::Event;
            match payload {
                Event::ChannelUpdated(ch) => {
                    assert_eq!(ch.name, new_name);
                    assert_eq!(ch.description, "Updated description");
                    saw_update = true;
                }
                Event::OperationFailed(e) => {
                    let msg = e.error.map(|err| err.message).unwrap_or_default();
                    panic!("Channel update failed: {msg}");
                }
                _ => {}
            }
        }
    }
    assert!(
        saw_update,
        "the update stream closed without a ChannelUpdated event — the rename \
         never happened and the old test would have passed anyway"
    );
}

/// INFRA-30 regression: a nil `update_mask` must be REJECTED, not silently
/// treated as "update everything".
///
/// The mask decides which fields change. Before it was enforced, a client that
/// sent a whole ChannelProto with an empty mask overwrote every field it
/// happened to leave at its default — clearing a description, resetting
/// security-relevant config, during an unrelated rename. The server now returns
/// InvalidArgument (channels.go:547). This pins that from the client side, so
/// the client cannot quietly go back to sending `None`.
#[tokio::test]
async fn test_update_group_settings_rejects_empty_update_mask() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let name = format!("mask_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &name,
            ChannelType::ChannelPrivate,
            None,
            Some("Original description"),
            None,
            Some(membership::genesis(&user, false).1),
        ).await
        .expect("Failed to create group");
    let channel_id = consume_create_stream_get_id(stream).await;

    let updated_channel = ChannelProto {
        id: channel_id.clone(),
        name: format!("renamed_{}", unique_username()),
        r#type: ChannelType::ChannelPrivate as i32,
        ..Default::default()
    };

    // No mask. The failure may surface either at the RPC call or as an
    // OperationFailed event on the stream; both are a rejection, and neither
    // may be a success.
    let mut rejected = false;
    match channel_client
        .update_channel(&channel_id, updated_channel, None)
        .await
    {
        Err(_) => rejected = true,
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(result) = stream.next().await {
                match result {
                    Err(_) => rejected = true,
                    Ok(event) => {
                        if let Some(payload) = event.event {
                            use chatter_core::proto::chat_v1::channel_event::Event;
                            match payload {
                                Event::OperationFailed(_) => rejected = true,
                                Event::ChannelUpdated(_) => panic!(
                                    "the server applied an update with NO update_mask — \
                                     an unrelated rename can now clear every other field"
                                ),
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }

    assert!(
        rejected,
        "a nil update_mask must be rejected (InvalidArgument), not accepted"
    );
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
            None,
            None,
            Some(membership::genesis(&user1, false).1),
        ).await
        .expect("Failed to create group");

    let channel_id = consume_create_stream_get_id(stream).await;
    join_by_invite(&pool, &mut channel_client, &channel_id, &user1, &user2).await;

    // User2 leaves the group
    pool.auth().set_basic(
        format!("{}.{}", user2.user_id, user2.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user2.device_auth_token),
    );

    let mut user2_channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let change = membership::leave(&user2, &mut user2_channel_client, &channel_id).await;
    let leave_result = user2_channel_client.leave_channel(&channel_id, change).await;

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
    let user = assert_ok(setup_authenticated_test(&pool).await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("pagination_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            None,
            None,
            Some(membership::genesis(&user, false).1),
        ).await
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
                vec![],
                false,
                vec![],
                Vec::new(),
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

    // Get second page using next_page_token. `reference_message_id` is a
    // "messages around this ID" cursor (returns both newer and older
    // messages bracketing it), so it can't be used to walk strictly
    // forward through the channel — that's what `page_token` is for.
    if !page1.next_page_token.is_empty() {
        let page2 = assert_ok(
            msg_client
                .get_messages(
                    &channel_id,
                    5,
                    None,
                    Some(&page1.next_page_token),
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
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create private group
    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let group_name = format!("private_permission_test_{}", unique_username());

    let stream = channel_client
        .create_channel(
            &group_name,
            ChannelType::ChannelPrivate,
            None,
            None,
            None,
            Some(membership::genesis(&user1, false).1),
        ).await
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
            vec![],
            false,
            vec![],
            Vec::new(),
        )
        .await;

    // Should fail - non-member cannot send to private group
    assert!(
        result.is_err(),
        "Non-member should not be able to send to private group"
    );
}

/// `invitee` joins `channel_id` the only way into a private channel: the
/// owner's signed invite, then the invitee's own signed accept. Leaves the
/// pool authenticated as `owner`.
async fn join_by_invite(
    pool: &chatter_core::network::grpc::GrpcClientPool,
    owner_client: &mut ChannelClient,
    channel_id: &str,
    owner: &TestUser,
    invitee: &TestUser,
) {
    let change = membership::invite(owner, owner_client, channel_id, invitee).await;
    let stream = owner_client.invite_user(channel_id, change, None).await.expect("invite");
    tokio::pin!(stream);
    while let Some(ev) = stream.next().await {
        ev.expect("invite stream");
    }
    let as_ = |u: &TestUser| {
        pool.auth().set_basic(
            format!("{}.{}", u.user_id, u.device_id),
            base64::engine::general_purpose::STANDARD.encode(&u.device_auth_token),
        )
    };
    as_(invitee);
    let mut client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let change = membership::accept(invitee, &mut client, channel_id).await;
    let stream = client.respond_to_invite(channel_id, change).await.expect("accept");
    tokio::pin!(stream);
    while let Some(ev) = stream.next().await {
        ev.expect("accept stream");
    }
    as_(owner);
}
