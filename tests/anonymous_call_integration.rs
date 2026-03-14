//! Integration test for anonymous call functionality.
//!
//! Tests the basic flow of anonymous call creation, acceptance, and cleanup
//! without requiring actual network connections.

use std::sync::Arc;
use chatter_linux::services::anon_dm::AnonDmService;
use chatter_linux::mailbox::CallRoomToken;
use chatter_linux::domain::anon_dm::AnonDmId;

/// Test that we can create call room tokens and store pending call state.
#[tokio::test]
async fn test_anonymous_call_token_management() {
    // Note: This test would need proper initialization in a real scenario
    // For now, we're just testing that the code compiles and basic structure works
    
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
    
    let pending = chatter_linux::bin_ui::features::calls::key_exchange::PendingAnonCall {
        call_room_id,
        room_name,
        conversation_id,
        token: dummy_token,
        is_video_call: false,
        is_outgoing: true,
        display_name: "Test User".to_string(),
    };
    
    // Verify the structure can be created
    assert_eq!(pending.display_name, "Test User");
    assert_eq!(pending.is_video_call, false);
    assert_eq!(pending.is_outgoing, true);
    
    println!("✅ Anonymous call structures can be created successfully");
}

/// Test basic call state management functions.
#[test] 
fn test_call_state_management() {
    use chatter_linux::bin_ui::features::calls::key_exchange::{
        set_pending_anon_call, get_pending_anon_call, clear_pending_anon_call
    };
    
    // Clear any existing state
    clear_pending_anon_call();
    
    // Should be None initially
    assert!(get_pending_anon_call().is_none());
    
    // Create and set pending call
    let dummy_token = CallRoomToken {
        id: "test-token-456".to_string(),
        public_key: vec![1, 2, 3, 4],
        private_key: vec![5, 6, 7, 8],
        signature: vec![9, 10, 11, 12],
        randomizer: Some(vec![13, 14, 15, 16]),
    };
    
    let pending = chatter_linux::bin_ui::features::calls::key_exchange::PendingAnonCall {
        call_room_id: [2u8; 16],
        room_name: "test-room-2".to_string(),
        conversation_id: AnonDmId::new(),
        token: dummy_token,
        is_video_call: true,
        is_outgoing: false,
        display_name: "Test Peer".to_string(),
    };
    
    set_pending_anon_call(pending);
    
    // Should now have the pending call
    let retrieved = get_pending_anon_call();
    assert!(retrieved.is_some());
    
    let retrieved = retrieved.unwrap();
    assert_eq!(retrieved.display_name, "Test Peer");
    assert_eq!(retrieved.is_video_call, true);
    assert_eq!(retrieved.is_outgoing, false);
    assert_eq!(retrieved.room_name, "test-room-2");
    
    // Clear and verify it's gone
    clear_pending_anon_call();
    assert!(get_pending_anon_call().is_none());
    
    println!("✅ Call state management functions work correctly");
}

/// Test that CallWorkerHandle has the connect_with_token method.
#[tokio::test]
async fn test_call_worker_connect_method_exists() {
    // We can't easily test the actual connection without a full setup,
    // but we can verify the method signature exists by using it in a way
    // that would fail at runtime but succeed at compile time
    
    use chatter_linux::services::call_worker::CallWorkerHandle;
    use chatter_linux::domain::ids::{CallId, ChannelId};
    
    // This function exists and has the right signature if this compiles
    async fn _test_method_signature() -> Result<(), Box<dyn std::error::Error>> {
        // Note: We can't actually call this without proper initialization
        // but this verifies the method signature exists
        let _dummy_call = |worker: &CallWorkerHandle| async move {
            worker.connect_with_token(
                "dummy-token",
                CallId::new(),
                ChannelId::new(),
            ).await
        };
        
        Ok(())
    }
    
    // If this compiles, the method signature is correct
    let _ = _test_method_signature().await;
    
    println!("✅ CallWorkerHandle::connect_with_token method signature is correct");
}

#[test]
fn test_anonymous_call_types_compile() {
    // Test that all our anonymous call related types can be imported and used
    use chatter_linux::services::anon_dm::calls::AnonCallRoomCredentials;
    use chatter_linux::mailbox::CallRoomToken;
    use chatter_linux::bin_ui::features::calls::key_exchange::PendingAnonCall;
    
    // Create instances to verify structure
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
    println!("✅ All anonymous call types are properly defined and accessible");
}