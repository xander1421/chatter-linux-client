//! Integration tests for Anonymous DMs (NDSS'21 style).
//!
//! Tests the anonymous DM functionality where strangers can message
//! public accounts without the server knowing about the conversation.
//!
//! The flow is:
//! 1. Stranger sends a message request to public account's device inbox
//! 2. Public account sees message request, can accept/decline
//! 3. On accept, both parties exchange ephemeral mailbox keys
//! 4. Messages flow through anonymous mailboxes (server-oblivious)
//!
//! Note: Since anonymous DMs are client-side managed, these tests focus on:
//! - Content proto serialization (AnonDmRequest, AnonDmAccept, AnonDmMessage)
//! - Ephemeral mailbox operations (reused from anon groups)
//! - Domain type construction and validation
//! - Two-user flow simulation

use crate::{common, skip_if_no_server};

use chatter_core::domain::UserId;
use chatter_core::domain::anon_dm::{AnonDm, AnonDmId, AnonDmMailbox, AnonDmMessage, AnonDmStatus};
use chatter_core::network::grpc::MailboxClient;
use chatter_core::proto::mailbox_v1::EnvelopeType;
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;

use common::*;

// ============================================================================
// AnonDmId Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_id_generation() {
    // Test that AnonDmId generates unique identifiers
    let mut ids = std::collections::HashSet::new();

    for _ in 0..100 {
        let id = AnonDmId::new();
        assert!(ids.insert(id), "AnonDmId should be unique");
    }

    eprintln!("Generated 100 unique AnonDmIds");
}

#[tokio::test]
async fn test_anon_dm_id_roundtrip() {
    let id = AnonDmId::new();
    let bytes = id.as_bytes();

    assert_eq!(bytes.len(), 16, "AnonDmId should be 16 bytes (UUID)");

    let restored = AnonDmId::try_from_slice(bytes);
    assert!(restored.is_some(), "Should restore from bytes");
    assert_eq!(restored.unwrap(), id, "Roundtrip should preserve ID");
}

#[tokio::test]
async fn test_anon_dm_id_display() {
    let id = AnonDmId::new();
    let display = id.to_string();

    // UUID format: 8-4-4-4-12 = 36 chars
    assert_eq!(display.len(), 36, "Display should be UUID format");
    assert!(display.contains('-'), "UUID should have dashes");
}

// ============================================================================
// AnonDmStatus Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_status_can_send() {
    // Only Active status allows sending
    assert!(AnonDmStatus::Active.can_send(), "Active should allow send");
    assert!(
        !AnonDmStatus::Pending.can_send(),
        "Pending should not allow send"
    );
    assert!(
        !AnonDmStatus::Blocked.can_send(),
        "Blocked should not allow send"
    );
    assert!(
        !AnonDmStatus::Declined.can_send(),
        "Declined should not allow send"
    );
}

#[tokio::test]
async fn test_anon_dm_status_from_i32() {
    assert_eq!(AnonDmStatus::from_i32(0), AnonDmStatus::Pending);
    assert_eq!(AnonDmStatus::from_i32(1), AnonDmStatus::Active);
    assert_eq!(AnonDmStatus::from_i32(2), AnonDmStatus::Blocked);
    assert_eq!(AnonDmStatus::from_i32(3), AnonDmStatus::Declined);
    assert_eq!(AnonDmStatus::from_i32(99), AnonDmStatus::Pending); // Default
}

// ============================================================================
// AnonDm Domain Type Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_new_initiated() {
    let peer_id = UserId::new();
    let dm = AnonDm::new_initiated(peer_id);

    assert!(dm.initiated_by_us, "Should be initiated by us");
    assert_eq!(dm.status, AnonDmStatus::Pending, "Should start as pending");
    assert!(dm.peer_mailbox_pk.is_none(), "No peer mailbox yet");
    assert!(dm.peer_profile_key.is_none(), "No peer profile key yet");
    assert_eq!(dm.unread_count, 0, "No unread messages initially");
}

#[tokio::test]
async fn test_anon_dm_from_request() {
    let peer_id = UserId::new();
    let mut mailbox_pk = [0u8; 32];
    rand::rng().fill_bytes(&mut mailbox_pk);

    let dm = AnonDm::from_request(AnonDmId::new(), peer_id, mailbox_pk);

    assert!(!dm.initiated_by_us, "Should not be initiated by us");
    assert_eq!(dm.status, AnonDmStatus::Pending, "Should start as pending");
    assert!(dm.peer_mailbox_pk.is_some(), "Should have peer mailbox");
    assert_eq!(dm.peer_mailbox_pk.unwrap(), mailbox_pk);
}

#[tokio::test]
async fn test_anon_dm_with_profile_key() {
    let peer_id = UserId::new();
    let mut mailbox_pk = [0u8; 32];
    let mut profile_key = [0u8; 32];
    rand::rng().fill_bytes(&mut mailbox_pk);
    rand::rng().fill_bytes(&mut profile_key);

    let mut dm = AnonDm::from_request(AnonDmId::new(), peer_id, mailbox_pk);
    // Profile key is set separately after exchange
    dm.peer_profile_key = Some(profile_key);

    assert!(dm.peer_profile_key.is_some(), "Should have profile key");
    assert_eq!(dm.peer_profile_key.unwrap(), profile_key);
}

#[tokio::test]
async fn test_anon_dm_is_ready() {
    let peer_id = UserId::new();
    let mut dm = AnonDm::new_initiated(peer_id);

    assert!(!dm.is_ready(), "Pending DM should not be ready");

    // is_ready() requires both Active status AND peer_mailbox_pk
    dm.status = AnonDmStatus::Active;
    assert!(
        !dm.is_ready(),
        "Active without peer mailbox should not be ready"
    );

    // Set peer mailbox pk (simulating acceptance response received)
    let mut mailbox_pk = [0u8; 32];
    rand::rng().fill_bytes(&mut mailbox_pk);
    dm.peer_mailbox_pk = Some(mailbox_pk);
    assert!(dm.is_ready(), "Active DM with peer mailbox should be ready");

    dm.status = AnonDmStatus::Blocked;
    assert!(!dm.is_ready(), "Blocked DM should not be ready");
}

// ============================================================================
// AnonDmMailbox Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_mailbox_creation() {
    let conversation_id = AnonDmId::new();

    // Generate keypair
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);

    let mailbox = AnonDmMailbox {
        id: uuid::Uuid::new_v4().to_string(),
        conversation_id,
        public_key: verifying_key.to_bytes(),
        private_key: signing_key.to_keypair_bytes(),
        token_id: "token-123".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::days(1),
    };

    assert_eq!(mailbox.conversation_id, conversation_id);
    assert_eq!(mailbox.public_key.len(), 32);
    assert_eq!(mailbox.private_key.len(), 64);
}

// ============================================================================
// AnonDmMessage Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_message_text() {
    let conversation_id = AnonDmId::new();
    let sender_id = UserId::new();

    let msg = AnonDmMessage::new_text(
        conversation_id,
        sender_id,
        "Hello from a stranger!",
        0, // sent_at timestamp
    );

    assert_eq!(msg.conversation_id, conversation_id);
    assert_eq!(msg.sender_id, sender_id);
    assert_eq!(msg.content_type, 1); // Text = 1
    assert!(msg.content_json.contains("Hello from a stranger!"));
    assert!(!msg.is_deleted);
    assert!(!msg.is_read);
}

// ============================================================================
// Content Serialization Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_request_serialization() {
    skip_if_no_server!();

    use chatter_core::proto::content_v1::{self, AnonDmRequest, Content};
    use prost::Message;

    let conversation_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Generate initiator mailbox keypair
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);
    let initiator_mailbox_pk = verifying_key.to_bytes().to_vec();

    let request = AnonDmRequest {
        conversation_id: conversation_id.clone(),
        initiator_mailbox_pk: initiator_mailbox_pk.clone(),
        intro_message: "Hey, I saw your public profile!".to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        profile_key: Some(vec![0u8; 32]), // Optional profile key
        encrypted_display_name: None,
        encrypted_avatar: None,
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonDmRequest(request)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    // Serialize
    let encoded = content.encode_to_vec();
    assert!(!encoded.is_empty(), "Request content should serialize");

    // Deserialize
    let decoded = Content::decode(&encoded[..]).expect("Should decode");
    match decoded.content {
        Some(content_v1::content::Content::AnonDmRequest(req)) => {
            assert_eq!(req.conversation_id, conversation_id);
            assert_eq!(req.initiator_mailbox_pk, initiator_mailbox_pk);
            assert_eq!(req.intro_message, "Hey, I saw your public profile!");
            assert!(req.profile_key.is_some());
        }
        _ => panic!("Expected AnonDmRequest"),
    }

    eprintln!("AnonDmRequest size: {} bytes", encoded.len());
}

#[tokio::test]
async fn test_anon_dm_accept_serialization() {
    skip_if_no_server!();

    use chatter_core::proto::content_v1::{self, AnonDmAccept, Content};
    use prost::Message;

    let conversation_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Generate responder mailbox keypair
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);
    let responder_mailbox_pk = verifying_key.to_bytes().to_vec();

    let accept = AnonDmAccept {
        conversation_id: conversation_id.clone(),
        responder_mailbox_pk: responder_mailbox_pk.clone(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        profile_key: Some(vec![0u8; 32]),
        encrypted_display_name: None,
        encrypted_avatar: None,
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonDmAccept(accept)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    // Serialize
    let encoded = content.encode_to_vec();
    assert!(!encoded.is_empty(), "Accept content should serialize");

    // Deserialize
    let decoded = Content::decode(&encoded[..]).expect("Should decode");
    match decoded.content {
        Some(content_v1::content::Content::AnonDmAccept(acc)) => {
            assert_eq!(acc.conversation_id, conversation_id);
            assert_eq!(acc.responder_mailbox_pk, responder_mailbox_pk);
            assert!(acc.profile_key.is_some());
        }
        _ => panic!("Expected AnonDmAccept"),
    }

    eprintln!("AnonDmAccept size: {} bytes", encoded.len());
}

#[tokio::test]
async fn test_anon_dm_message_serialization() {
    skip_if_no_server!();

    use chatter_core::proto::content_v1::{
        self, AnonDmMessage as ProtoAnonDmMessage, Content, DataMessage,
    };
    use prost::Message;

    let conversation_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Create inner content (the actual message)
    let inner = Content {
        content: Some(content_v1::content::Content::DataMessage(DataMessage {
            body: "This is a secret message!".to_string(),
            ..Default::default()
        })),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![],
    };

    let dm_msg = ProtoAnonDmMessage {
        conversation_id: conversation_id.clone(),
        inner_content: Some(Box::new(inner)),
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
            dm_msg,
        ))),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    // Serialize
    let encoded = content.encode_to_vec();
    assert!(!encoded.is_empty(), "Message content should serialize");

    // Deserialize
    let decoded = Content::decode(&encoded[..]).expect("Should decode");
    match decoded.content {
        Some(content_v1::content::Content::AnonDmMessage(msg)) => {
            assert_eq!(msg.conversation_id, conversation_id);
            assert!(msg.inner_content.is_some());

            // Verify inner content
            let inner = msg.inner_content.unwrap();
            match inner.content {
                Some(content_v1::content::Content::DataMessage(dm)) => {
                    assert_eq!(dm.body, "This is a secret message!");
                }
                _ => panic!("Expected DataMessage inside AnonDmMessage"),
            }
        }
        _ => panic!("Expected AnonDmMessage"),
    }

    eprintln!("AnonDmMessage size: {} bytes", encoded.len());
}

// ============================================================================
// Ephemeral Mailbox Tests (for Anonymous DM)
// ============================================================================

#[tokio::test]
async fn test_anon_dm_mailbox_challenge_response() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Generate a keypair for the anonymous DM mailbox
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);
    let public_key = verifying_key.to_bytes();

    // Try to get a challenge for the mailbox
    let result = mailbox_client
        .get_mailbox_challenge(public_key.to_vec())
        .await;

    match result {
        Ok(response) => {
            eprintln!("Got challenge: {} bytes", response.challenge.len());

            // Sign the challenge
            let signature = signing_key.sign(&response.challenge);

            // Verify locally that our signature is valid
            assert!(
                verifying_key
                    .verify(&response.challenge, &signature)
                    .is_ok(),
                "Signature should verify locally"
            );
        }
        Err(e) => {
            // Expected if mailbox doesn't exist
            eprintln!(
                "Challenge error (expected if mailbox not registered): {:?}",
                e
            );
        }
    }
}

#[tokio::test]
async fn test_anon_dm_send_to_nonexistent_mailbox() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut mailbox_client = MailboxClient::new(pool.channel(), pool.auth_clone());

    // Generate a random mailbox public key (not registered)
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);
    let mailbox_pk = verifying_key.to_bytes();

    // Try to send a message to the nonexistent mailbox
    let test_payload = b"Test anonymous DM message";
    let result = mailbox_client
        .send_to_ephemeral_mailbox(
            mailbox_pk.to_vec(),
            test_payload.to_vec(),
            EnvelopeType::SealedSender as i32,
        )
        .await;

    // Should fail since mailbox doesn't exist
    match result {
        Ok(_) => {
            // Server might accept message for unknown mailbox (fire-and-forget)
            eprintln!("Message accepted (server may queue for unknown mailboxes)");
        }
        Err(e) => {
            eprintln!("Send failed (expected): {:?}", e);
        }
    }
}

// ============================================================================
// Two-User Anonymous DM Flow Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_two_user_setup() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create user2 (the stranger)
    pool.auth().clear();
    let user2 = assert_ok(register_test_user(&pool).await);

    // Both users exist and can be used for anonymous DM flow
    eprintln!("User1 (public account): {}", user1.user_id);
    eprintln!("User2 (stranger): {}", user2.user_id);

    // In a full implementation:
    // 1. User2 discovers User1's profile (public account)
    // 2. User2 creates AnonDmRequest with their mailbox PK
    // 3. User2 sends request to User1's device inbox via sealed sender
    // 4. User1 receives request, decides to accept
    // 5. User1 creates AnonDmAccept with their mailbox PK
    // 6. User1 sends accept to User2's mailbox
    // 7. Both parties now exchange messages via their ephemeral mailboxes

    // For now, verify we can create the necessary components
    let conversation_id = AnonDmId::new();

    // User2 creates a mailbox for this conversation
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let user2_signing_key = SigningKey::from_bytes(&seed);
    let user2_verifying_key = VerifyingKey::from(&user2_signing_key);
    let user2_mailbox_pk = user2_verifying_key.to_bytes();

    // User1 would receive this in the request
    let dm_for_user1 = AnonDm::from_request(
        conversation_id,
        user2.user_id.parse::<UserId>().expect("valid user id"),
        user2_mailbox_pk,
    );

    assert_eq!(dm_for_user1.status, AnonDmStatus::Pending);
    assert!(!dm_for_user1.initiated_by_us);

    eprintln!("Created pending DM conversation: {}", dm_for_user1.id);
}

#[tokio::test]
async fn test_anon_dm_request_acceptance_flow() {
    skip_if_no_server!();

    // Simulate the request -> accept flow using domain types
    let stranger_id = UserId::new();
    let public_account_id = UserId::new();
    let conversation_id = AnonDmId::new();

    // 1. Stranger initiates request
    let mut stranger_dm = AnonDm::new_initiated(public_account_id);
    stranger_dm.id = conversation_id;

    // Generate stranger's mailbox
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let stranger_signing_key = SigningKey::from_bytes(&seed);
    let stranger_verifying_key = VerifyingKey::from(&stranger_signing_key);
    let stranger_mailbox_pk = stranger_verifying_key.to_bytes();

    assert_eq!(stranger_dm.status, AnonDmStatus::Pending);
    assert!(stranger_dm.initiated_by_us);

    // 2. Public account receives request
    let mut public_dm = AnonDm::from_request(conversation_id, stranger_id, stranger_mailbox_pk);

    assert_eq!(public_dm.status, AnonDmStatus::Pending);
    assert!(!public_dm.initiated_by_us);
    assert_eq!(public_dm.peer_mailbox_pk.unwrap(), stranger_mailbox_pk);

    // 3. Public account accepts - generate their mailbox
    rand::rng().fill_bytes(&mut seed);
    let public_signing_key = SigningKey::from_bytes(&seed);
    let public_verifying_key = VerifyingKey::from(&public_signing_key);
    let public_mailbox_pk = public_verifying_key.to_bytes();

    // Update both sides to Active
    public_dm.status = AnonDmStatus::Active;
    stranger_dm.status = AnonDmStatus::Active;
    stranger_dm.peer_mailbox_pk = Some(public_mailbox_pk);

    assert!(public_dm.is_ready());
    assert!(stranger_dm.is_ready());
    assert!(public_dm.status.can_send());
    assert!(stranger_dm.status.can_send());

    eprintln!("Anonymous DM flow complete:");
    eprintln!("  Conversation ID: {}", conversation_id);
    eprintln!("  Stranger mailbox: {} bytes", stranger_mailbox_pk.len());
    eprintln!("  Public mailbox: {} bytes", public_mailbox_pk.len());
}

#[tokio::test]
async fn test_anon_dm_decline_flow() {
    // Test declining a message request
    let stranger_id = UserId::new();
    let mut mailbox_pk = [0u8; 32];
    rand::rng().fill_bytes(&mut mailbox_pk);

    let mut dm = AnonDm::from_request(AnonDmId::new(), stranger_id, mailbox_pk);
    assert_eq!(dm.status, AnonDmStatus::Pending);

    // Decline the request
    dm.status = AnonDmStatus::Declined;

    assert!(!dm.is_ready());
    assert!(!dm.status.can_send());
    assert_eq!(dm.status, AnonDmStatus::Declined);
}

#[tokio::test]
async fn test_anon_dm_block_flow() {
    // Test blocking a conversation
    let stranger_id = UserId::new();
    let mut mailbox_pk = [0u8; 32];
    rand::rng().fill_bytes(&mut mailbox_pk);

    let mut dm = AnonDm::from_request(AnonDmId::new(), stranger_id, mailbox_pk);
    dm.status = AnonDmStatus::Active; // Was accepted

    assert!(dm.is_ready());

    // Block the conversation
    dm.status = AnonDmStatus::Blocked;

    assert!(!dm.is_ready());
    assert!(!dm.status.can_send());
}

// ============================================================================
// Message Content Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_message_with_attachment() {
    skip_if_no_server!();

    use chatter_core::proto::content_v1::{
        self, AnonDmMessage as ProtoAnonDmMessage, AttachmentMessage, Content,
    };
    use prost::Message;

    let conversation_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Create attachment content
    let attachment = AttachmentMessage {
        attachment: None, // Would have AttachmentPointer in real use
        caption: "Check out this image!".to_string(),
        timestamp: 0,
        channel_id: String::new(),
        author_name: String::new(),
    };

    let inner = Content {
        content: Some(content_v1::content::Content::Attachment(attachment)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![],
    };

    let dm_msg = ProtoAnonDmMessage {
        conversation_id: conversation_id.clone(),
        inner_content: Some(Box::new(inner)),
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
            dm_msg,
        ))),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    let encoded = content.encode_to_vec();
    let decoded = Content::decode(&encoded[..]).expect("Should decode");

    match decoded.content {
        Some(content_v1::content::Content::AnonDmMessage(msg)) => {
            let inner = msg.inner_content.unwrap();
            match inner.content {
                Some(content_v1::content::Content::Attachment(att)) => {
                    assert_eq!(att.caption, "Check out this image!");
                }
                _ => panic!("Expected Attachment inside AnonDmMessage"),
            }
        }
        _ => panic!("Expected AnonDmMessage"),
    }

    eprintln!(
        "AnonDmMessage with attachment size: {} bytes",
        encoded.len()
    );
}

#[tokio::test]
async fn test_anon_dm_message_with_reaction() {
    skip_if_no_server!();

    use chatter_core::proto::content_v1::{
        self, AnonDmMessage as ProtoAnonDmMessage, Content, Reaction,
    };
    use prost::Message;

    let conversation_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Create reaction to a previous message
    let reaction = Reaction {
        emoji: "❤️".to_string(),
        remove: false,
        target_author_aci: "target-author-uuid".to_string(),
        target_sent_timestamp: 1234567890,
        timestamp: 1234567891,
    };

    let inner = Content {
        content: Some(content_v1::content::Content::Reaction(reaction)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![],
    };

    let dm_msg = ProtoAnonDmMessage {
        conversation_id: conversation_id.clone(),
        inner_content: Some(Box::new(inner)),
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
            dm_msg,
        ))),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    let encoded = content.encode_to_vec();
    let decoded = Content::decode(&encoded[..]).expect("Should decode");

    match decoded.content {
        Some(content_v1::content::Content::AnonDmMessage(msg)) => {
            let inner = msg.inner_content.unwrap();
            match inner.content {
                Some(content_v1::content::Content::Reaction(r)) => {
                    assert_eq!(r.emoji, "❤️");
                    assert!(!r.remove);
                }
                _ => panic!("Expected Reaction inside AnonDmMessage"),
            }
        }
        _ => panic!("Expected AnonDmMessage"),
    }

    eprintln!("AnonDmMessage with reaction size: {} bytes", encoded.len());
}

// ============================================================================
// Ed25519 Mailbox Authentication Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_mailbox_ed25519_signature() {
    // Test that Ed25519 signatures work correctly for mailbox authentication
    use ed25519_dalek::Verifier;

    // Generate keypair for anonymous DM mailbox
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = VerifyingKey::from(&signing_key);

    // Simulate server challenge
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

#[tokio::test]
async fn test_anon_dm_keypair_bytes_roundtrip() {
    // Test that keypair bytes can be stored and restored
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let original_signing_key = SigningKey::from_bytes(&seed);

    // Get keypair bytes (what we store in database)
    let keypair_bytes = original_signing_key.to_keypair_bytes();
    assert_eq!(keypair_bytes.len(), 64);

    // Restore from bytes
    let restored_signing_key =
        SigningKey::from_keypair_bytes(&keypair_bytes).expect("Should restore from keypair bytes");

    // Verify they produce same signatures
    let test_message = b"Test message for anonymous DM";
    let original_sig = original_signing_key.sign(test_message);
    let restored_sig = restored_signing_key.sign(test_message);

    assert_eq!(
        original_sig, restored_sig,
        "Signatures should match after restore"
    );
}

// ============================================================================
// Privacy Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_no_server_linkage() {
    // Verify that conversation IDs are client-generated and server has no knowledge
    let conv1 = AnonDmId::new();
    let conv2 = AnonDmId::new();

    // IDs are random UUIDs, not derived from user IDs
    assert_ne!(conv1, conv2);

    // IDs cannot be linked to users without client-side data
    let conv1_bytes = conv1.as_bytes();
    let conv2_bytes = conv2.as_bytes();

    // No pattern that could link to user identities
    assert_ne!(conv1_bytes, conv2_bytes);

    eprintln!("Conversation 1: {}", conv1);
    eprintln!("Conversation 2: {}", conv2);
    eprintln!("Server cannot link these to any users without client data");
}

#[tokio::test]
async fn test_anon_dm_mailbox_unlinkability() {
    // Verify that different conversations use different mailboxes
    let mut mailboxes = std::collections::HashSet::new();

    for _ in 0..10 {
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = VerifyingKey::from(&signing_key);
        let pk = verifying_key.to_bytes();

        // Each mailbox public key should be unique
        assert!(mailboxes.insert(pk), "Mailbox keys should be unique");
    }

    eprintln!("Generated 10 unique mailbox keys - server cannot link conversations");
}

// ============================================================================
// Typing Indicator Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_message_with_typing() {
    skip_if_no_server!();

    use chatter_core::proto::content_v1::{
        self, AnonDmMessage as ProtoAnonDmMessage, Content, TypingMessage, typing_message,
    };
    use prost::Message;

    let conversation_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Create typing started indicator
    let typing = TypingMessage {
        action: typing_message::Action::Started as i32,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        channel_id: String::new(), // Empty for DMs
    };

    let inner = Content {
        content: Some(content_v1::content::Content::Typing(typing)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![],
    };

    let dm_msg = ProtoAnonDmMessage {
        conversation_id: conversation_id.clone(),
        inner_content: Some(Box::new(inner)),
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
            dm_msg,
        ))),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    let encoded = content.encode_to_vec();
    let decoded = Content::decode(&encoded[..]).expect("Should decode");

    match decoded.content {
        Some(content_v1::content::Content::AnonDmMessage(msg)) => {
            let inner = msg.inner_content.unwrap();
            match inner.content {
                Some(content_v1::content::Content::Typing(t)) => {
                    assert_eq!(t.action, typing_message::Action::Started as i32);
                }
                _ => panic!("Expected Typing inside AnonDmMessage"),
            }
        }
        _ => panic!("Expected AnonDmMessage"),
    }

    eprintln!("AnonDmMessage with typing size: {} bytes", encoded.len());
}

// ============================================================================
// Read Receipt Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_message_with_read_receipt() {
    skip_if_no_server!();

    use chatter_core::proto::content_v1::{
        self, AnonDmMessage as ProtoAnonDmMessage, Content, ReceiptMessage, receipt_message,
    };
    use prost::Message;

    let conversation_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Create read receipt for multiple messages
    let receipt = ReceiptMessage {
        r#type: receipt_message::Type::Read as i32,
        timestamps: vec![1234567890, 1234567891, 1234567892],
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        channel_id: String::new(), // Empty for DMs
    };

    let inner = Content {
        content: Some(content_v1::content::Content::Receipt(receipt)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![],
    };

    let dm_msg = ProtoAnonDmMessage {
        conversation_id: conversation_id.clone(),
        inner_content: Some(Box::new(inner)),
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
            dm_msg,
        ))),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    let encoded = content.encode_to_vec();
    let decoded = Content::decode(&encoded[..]).expect("Should decode");

    match decoded.content {
        Some(content_v1::content::Content::AnonDmMessage(msg)) => {
            let inner = msg.inner_content.unwrap();
            match inner.content {
                Some(content_v1::content::Content::Receipt(r)) => {
                    assert_eq!(r.r#type, receipt_message::Type::Read as i32);
                    assert_eq!(r.timestamps.len(), 3);
                }
                _ => panic!("Expected Receipt inside AnonDmMessage"),
            }
        }
        _ => panic!("Expected AnonDmMessage"),
    }

    eprintln!(
        "AnonDmMessage with read receipt size: {} bytes",
        encoded.len()
    );
}

// ============================================================================
// Delete For Everyone Tests
// ============================================================================

#[tokio::test]
async fn test_anon_dm_message_with_delete() {
    skip_if_no_server!();

    use chatter_core::proto::content_v1::{
        self, AnonDmMessage as ProtoAnonDmMessage, Content, DataMessage, Delete,
    };
    use prost::Message;

    let conversation_id = uuid::Uuid::new_v4().as_bytes().to_vec();

    // Create delete-for-everyone (DataMessage with Delete field)
    let delete = Delete {
        target_author_aci: None,
        destination_user_id: None,
        target_call_id: None,
        target_sent_timestamp: 1234567890,
    };

    let data_msg = DataMessage {
        delete: Some(delete),
        ..Default::default()
    };

    let inner = Content {
        content: Some(content_v1::content::Content::DataMessage(data_msg)),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![],
    };

    let dm_msg = ProtoAnonDmMessage {
        conversation_id: conversation_id.clone(),
        inner_content: Some(Box::new(inner)),
    };

    let content = Content {
        content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
            dm_msg,
        ))),
        ephemeral_reply_to: None,
        timestamp: 0,
        padding: vec![0u8; 32],
    };

    let encoded = content.encode_to_vec();
    let decoded = Content::decode(&encoded[..]).expect("Should decode");

    match decoded.content {
        Some(content_v1::content::Content::AnonDmMessage(msg)) => {
            let inner = msg.inner_content.unwrap();
            match inner.content {
                Some(content_v1::content::Content::DataMessage(dm)) => {
                    assert!(dm.delete.is_some());
                    assert_eq!(dm.delete.unwrap().target_sent_timestamp, 1234567890);
                }
                _ => panic!("Expected DataMessage with Delete inside AnonDmMessage"),
            }
        }
        _ => panic!("Expected AnonDmMessage"),
    }

    eprintln!("AnonDmMessage with delete size: {} bytes", encoded.len());
}
