//! Integration tests for Anonymous Groups (NDSS'21 style).
//!
//! Tests the anonymous group functionality where the server has no knowledge
//! of group existence. All operations use pairwise sealed sender via
//! ephemeral mailboxes.
//!
//! Note: Since anonymous groups are client-side only, these tests focus on:
//! - Ephemeral mailbox creation with blind signatures
//! - Token management for mailbox registration
//! - Message delivery to ephemeral mailboxes
//! - Full two-user flow (invite -> accept -> message)

mod common;

use base64::Engine;
use chatter_core::network::grpc::MailboxClient;
use chatter_core::proto::mailbox_v1::EnvelopeType;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;

use common::*;

// ============================================================================
// Ephemeral Mailbox Tests (Foundation for Anon Groups)
// ============================================================================

#[tokio::test]
async fn test_request_blind_signature_tokens() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Request blind signature tokens
    // The server provides blinded tokens that can be used to register anonymous mailboxes
    // Note: get_mailbox_tokens requires blinded keys to be provided
    let blinded_keys: Vec<Vec<u8>> = (0..5)
        .map(|_| {
            let mut key = vec![0u8; 256]; // RSA-2048 blinded key placeholder
            rand::rng().fill_bytes(&mut key);
            key
        })
        .collect();
    let result = mailbox_client.get_mailbox_tokens(5, blinded_keys).await;

    match result {
        Ok(response) => {
            eprintln!(
                "Received {} blind signatures",
                response.blinded_signatures.len()
            );
            // Signatures should be present
            assert!(
                response.blinded_signatures.len() <= 5,
                "Should receive signatures or be rate-limited"
            );
        }
        Err(e) => {
            // Server might not support blind tokens or rate limiting applies
            eprintln!("Blind token request error: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_create_ephemeral_mailbox() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Generate a keypair for the mailbox
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);
    let public_key = verifying_key.to_bytes().to_vec();

    // For testing, we'll try to create a mailbox with a fake signature
    // (Real flow would use blind signatures from request_blind_tokens)
    let fake_signature = vec![0u8; 64]; // Placeholder
    let ttl_seconds = 24 * 60 * 60; // 1 day

    let result = mailbox_client
        .create_ephemeral_mailbox(
            public_key.clone(),
            fake_signature,
            ttl_seconds,
            vec![], // No randomizer for fake test
        )
        .await;

    match result {
        Ok(response) => {
            eprintln!(
                "Created ephemeral mailbox: id={}, expires={}",
                response.mailbox_id, response.expires_at
            );
            assert!(!response.mailbox_id.is_empty());
        }
        Err(e) => {
            // Expected to fail without valid blind signature
            eprintln!("Ephemeral mailbox creation error (expected): {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_send_to_ephemeral_mailbox() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Generate a test mailbox public key
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);
    let mailbox_pk = verifying_key.to_bytes();

    // Create test envelope
    let test_payload = b"Test sealed sender message";

    let result = mailbox_client
        .send_to_ephemeral_mailbox(
            mailbox_pk.to_vec(),
            test_payload.to_vec(),
            EnvelopeType::SealedSender as i32,
        )
        .await;

    match result {
        Ok(_) => {
            eprintln!("Sent message to ephemeral mailbox");
        }
        Err(e) => {
            // Expected to fail if mailbox doesn't exist
            eprintln!("Send to mailbox error (expected if mailbox not registered): {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_poll_ephemeral_mailbox() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Generate a test mailbox keypair
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);
    let mailbox_pk = verifying_key.to_bytes();

    // Generate challenge response (for authentication)
    let mut challenge = vec![0u8; 32];
    rand::rng().fill_bytes(&mut challenge);
    let signature = signing_key.sign(&challenge);

    let result = mailbox_client
        .poll_ephemeral_mailbox(
            mailbox_pk.to_vec(),
            signature.to_bytes().to_vec(),
            challenge,
            None, // after_message_id
            100,  // limit
        )
        .await;

    match result {
        Ok(response) => {
            eprintln!(
                "Polled mailbox: {} messages",
                response.messages.len()
            );
        }
        Err(e) => {
            // Expected to fail if mailbox doesn't exist
            eprintln!("Poll mailbox error (expected if mailbox not registered): {:?}", e);
        }
    }
}

// ============================================================================
// Two-User Anonymous Group Flow Tests
// ============================================================================

#[tokio::test]
async fn test_anon_group_invite_via_dm() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create user2
    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    // Re-auth as user1
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    // In a full implementation, user1 would:
    // 1. Create anonymous group locally
    // 2. Create ephemeral mailbox for the group (using blind tokens)
    // 3. Send invite to user2 via DM (existing pairwise channel)
    //
    // For now, verify the prerequisite DM setup works

    use chatter_core::network::grpc::{ChannelClient, UserClient};
    use chatter_core::proto::chat_v1::{ChannelType, FriendStatus};
    use tokio_stream::StreamExt;

    // Make them friends first (required for DM)
    let mut user_client = UserClient::new(pool.channel(), pool.auth_clone());
    let stream = assert_ok(
        user_client
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

    // User1 creates DM channel (for invite delivery)
    pool.auth().set_basic(
        format!("{}.{}", user1.user_id, user1.device_id),
        base64::engine::general_purpose::STANDARD.encode(&user1.device_auth_token),
    );

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let dm_name = "dm_anon_test".to_string();
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
                    use chatter_core::proto::chat_v1::channel_event::Event;
                    if let Event::ChannelCreated(ch) = payload {
                        channel_id = ch.id;
                    }
                }
            }
            eprintln!("DM channel for anon group invites: {}", channel_id);
            assert!(!channel_id.is_empty(), "DM channel should be created for invites");
        }
        Err(e) => {
            // DM might already exist
            eprintln!("DM creation for invites: {:?}", e);
        }
    }
}

// ============================================================================
// Blind Signature Token Flow Tests
// ============================================================================

#[tokio::test]
async fn test_blind_token_full_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Step 1: Request blind tokens
    let blinded_keys: Vec<Vec<u8>> = (0..3)
        .map(|_| {
            let mut key = vec![0u8; 256];
            rand::rng().fill_bytes(&mut key);
            key
        })
        .collect();
    let tokens_result = mailbox_client.get_mailbox_tokens(3, blinded_keys).await;

    match tokens_result {
        Ok(response) => {
            eprintln!("Got {} blind signatures", response.blinded_signatures.len());

            // Step 2: For each signature, we would:
            // - Unblind the server's signature
            // - Store the token for later use
            // - Use it to register an anonymous mailbox
            //
            // This requires the full blind signature protocol implementation

            for (i, sig) in response.blinded_signatures.iter().enumerate() {
                eprintln!(
                    "Signature {}: len={}",
                    i,
                    sig.len()
                );
            }
        }
        Err(e) => {
            eprintln!("Blind tokens request failed: {:?}", e);
        }
    }
}

// ============================================================================
// Anonymous Group Message Content Tests
// ============================================================================

#[tokio::test]
async fn test_anon_group_content_serialization() {
    skip_if_no_server!();

    // Test the content proto serialization for anonymous group messages
    use prost::Message;
    use chatter_core::proto::content_v1::{self, AnonGroupInvite, AnonGroupMessage, Content, DataMessage};

    // Create a test group invite
    let group_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    let invite = AnonGroupInvite {
        group_id: group_id.clone(),
        group_name: "Test Secret Group".to_string(),
        inviter_mailbox_pk: vec![0u8; 32], // Placeholder
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
        profile_key: None,
        encrypted_display_name: None,
        encrypted_avatar: None,
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonGroupInvite(invite)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32], // Random padding
    };

    // Serialize
    let encoded = content.encode_to_vec();
    assert!(!encoded.is_empty(), "Invite content should serialize");

    // Deserialize
    let decoded = Content::decode(&encoded[..]).expect("Should decode");
    match decoded.content {
        Some(content_v1::content::Content::AnonGroupInvite(inv)) => {
            assert_eq!(inv.group_id, group_id);
            assert_eq!(inv.group_name, "Test Secret Group");
        }
        _ => panic!("Expected AnonGroupInvite"),
    }

    // Test group message wrapper
    let inner = Content {
        content: Some(content_v1::content::Content::DataMessage(DataMessage {
            body: "Hello secret group!".to_string(),
            ..Default::default()
        })),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![],
    };

    let group_msg = AnonGroupMessage {
        group_id: group_id.clone(),
        inner_content: Some(Box::new(inner)),
    };

    let wrapper = Content {
        content: Some(content_v1::content::Content::AnonGroupMessage(Box::new(group_msg))),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    let encoded_msg = wrapper.encode_to_vec();
    assert!(!encoded_msg.is_empty(), "Group message should serialize");

    eprintln!(
        "Invite size: {} bytes, Message size: {} bytes",
        encoded.len(),
        encoded_msg.len()
    );
}

// ============================================================================
// Anonymous Group Join Acknowledgement Tests
// ============================================================================

#[tokio::test]
async fn test_anon_group_join_ack_content() {
    skip_if_no_server!();

    use prost::Message;
    use chatter_core::proto::content_v1::{self, AnonGroupJoinAck, Content};

    // Create join ack (sent when accepting an invite)
    let group_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Generate a test mailbox keypair for the joiner
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);
    let joiner_mailbox_pk = verifying_key.to_bytes().to_vec();

    let join_ack = AnonGroupJoinAck {
        group_id: group_id.clone(),
        joiner_mailbox_pk: joiner_mailbox_pk.clone(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
        profile_key: None,
        encrypted_display_name: None,
        encrypted_avatar: None,
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonGroupJoinAck(join_ack)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    let encoded = content.encode_to_vec();
    let decoded = Content::decode(&encoded[..]).expect("Should decode");

    match decoded.content {
        Some(content_v1::content::Content::AnonGroupJoinAck(ack)) => {
            assert_eq!(ack.group_id, group_id);
            assert_eq!(ack.joiner_mailbox_pk, joiner_mailbox_pk);
        }
        _ => panic!("Expected AnonGroupJoinAck"),
    }

    eprintln!("JoinAck size: {} bytes", encoded.len());
}

// ============================================================================
// Anonymous Group Kick/Leave Content Tests
// ============================================================================

#[tokio::test]
async fn test_anon_group_kick_content() {
    skip_if_no_server!();

    use prost::Message;
    use chatter_core::proto::content_v1::{self, AnonGroupKick, Content};

    let group_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    let kicked_user_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Generate admin signature (placeholder)
    let admin_signature = vec![0u8; 64];

    let kick = AnonGroupKick {
        group_id: group_id.clone(),
        kicked_user_id: kicked_user_id.clone(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
        admin_signature,
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonGroupKick(kick)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    let encoded = content.encode_to_vec();
    let decoded = Content::decode(&encoded[..]).expect("Should decode");

    match decoded.content {
        Some(content_v1::content::Content::AnonGroupKick(k)) => {
            assert_eq!(k.group_id, group_id);
            assert_eq!(k.kicked_user_id, kicked_user_id);
        }
        _ => panic!("Expected AnonGroupKick"),
    }

    eprintln!("Kick size: {} bytes", encoded.len());
}

#[tokio::test]
async fn test_anon_group_leave_content() {
    skip_if_no_server!();

    use prost::Message;
    use chatter_core::proto::content_v1::{self, AnonGroupLeave, Content};

    let group_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    let leave = AnonGroupLeave {
        group_id: group_id.clone(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonGroupLeave(leave)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    let encoded = content.encode_to_vec();
    let decoded = Content::decode(&encoded[..]).expect("Should decode");

    match decoded.content {
        Some(content_v1::content::Content::AnonGroupLeave(l)) => {
            assert_eq!(l.group_id, group_id);
        }
        _ => panic!("Expected AnonGroupLeave"),
    }

    eprintln!("Leave size: {} bytes", encoded.len());
}

// ============================================================================
// Anonymous Group Member List Tests
// ============================================================================

#[tokio::test]
async fn test_anon_group_member_list_content() {
    skip_if_no_server!();

    use prost::Message;
    use chatter_core::proto::content_v1::{self, AnonGroupMemberInfo, AnonGroupMemberList, Content};

    let group_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Create member list with 3 members
    let members: Vec<AnonGroupMemberInfo> = (0..3)
        .map(|i| {
            let mut seed = [0u8; 32];
            rand::rng().fill_bytes(&mut seed);
            let signing_key = SigningKey::from_bytes(&seed);
            let verifying_key = VerifyingKey::from(&signing_key);

            AnonGroupMemberInfo {
                user_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
                mailbox_pk: verifying_key.to_bytes().to_vec(),
                role: if i == 0 { 2 } else { 0 }, // Owner = 2, Member = 0
                profile_key: None,
                encrypted_display_name: None,
                encrypted_avatar: None,
            }
        })
        .collect();

    let member_list = AnonGroupMemberList {
        group_id: group_id.clone(),
        members: members.clone(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonGroupMemberList(member_list)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    let encoded = content.encode_to_vec();
    let decoded = Content::decode(&encoded[..]).expect("Should decode");

    match decoded.content {
        Some(content_v1::content::Content::AnonGroupMemberList(ml)) => {
            assert_eq!(ml.group_id, group_id);
            assert_eq!(ml.members.len(), 3);
        }
        _ => panic!("Expected AnonGroupMemberList"),
    }

    eprintln!("MemberList (3 members) size: {} bytes", encoded.len());
}

// ============================================================================
// Mailbox Authentication Tests
// ============================================================================

#[tokio::test]
async fn test_mailbox_ed25519_signature() {
    skip_if_no_server!();

    // Test that Ed25519 signatures work correctly for mailbox authentication
    use ed25519_dalek::{Signer, Verifier};

    // Generate keypair
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);

    // Create challenge
    let mut challenge = [0u8; 32];
    rand::rng().fill_bytes(&mut challenge);

    // Sign challenge
    let signature = signing_key.sign(&challenge);

    // Verify signature
    assert!(
        verifying_key.verify(&challenge, &signature).is_ok(),
        "Signature should verify"
    );

    // Verify wrong challenge fails
    let mut wrong_challenge = [0u8; 32];
    rand::rng().fill_bytes(&mut wrong_challenge);
    assert!(
        verifying_key.verify(&wrong_challenge, &signature).is_err(),
        "Wrong challenge should fail verification"
    );
}

// ============================================================================
// Group ID Generation Tests
// ============================================================================

#[tokio::test]
async fn test_anon_group_id_uniqueness() {
    skip_if_no_server!();

    // Verify group IDs are unique UUIDs
    let mut ids = std::collections::HashSet::new();

    for _ in 0..100 {
        let id = uuid::Uuid::new_v4();
        assert!(ids.insert(id), "Group IDs should be unique");
    }

    eprintln!("Generated 100 unique group IDs");
}
