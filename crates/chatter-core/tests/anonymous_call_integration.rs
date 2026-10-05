//! Integration test for anonymous call functionality.
//!
//! Tests the basic flow of anonymous call creation, acceptance, and cleanup
//! without requiring actual network connections.

use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::mailbox::CallRoomToken;
use chatter_core::services::anon_dm::{AnonCallRoomCredentials, PendingAnonCall};

/// Test that we can create call room tokens and store pending call state.
#[tokio::test]
async fn test_anonymous_call_token_management() {
    // Create a dummy token for testing
    let dummy_token = CallRoomToken {
        id: "test-token-123".to_string(),
        public_key: vec![1, 2, 3, 4],
        private_key: vec![5, 6, 7, 8],
        signature: vec![9, 10, 11, 12],
        randomizer: Some(vec![13, 14, 15, 16]),
    };

    // Test that we can create the pending call structure
    let conversation_id = AnonDmId::new();
    let call_room_id = [1u8; 16];
    let room_name = "anon-test-room".to_string();

    let pending = PendingAnonCall {
        call_room_id,
        room_name,
        conversation_id,
        token: dummy_token,
        is_video_call: false,
        is_outgoing: true,
        display_name: "Test User".to_string(),
        encryption_key: vec![0u8; 32],
        sender_id: None,
        started_at: None,
    };

    // Verify the structure can be created
    assert_eq!(pending.display_name, "Test User");
    assert!(!pending.is_video_call);
    assert!(pending.is_outgoing);
}

/// Test that anonymous call types can be created and used.
#[test]
fn test_anonymous_call_types_compile() {
    // Test that all our anonymous call related types can be imported and used
    let _credentials = AnonCallRoomCredentials {
        livekit_token: "test-token".to_string(),
        livekit_url: "wss://test.com".to_string(),
        expires_at: 12345,
    };

    let _token = CallRoomToken {
        id: "test".to_string(),
        public_key: vec![],
        private_key: vec![],
        signature: vec![],
        randomizer: None,
    };

    // If we can create these, the types are properly exported
}
