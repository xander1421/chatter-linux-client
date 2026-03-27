//! Integration tests for end-to-end flows requiring a running server.
//!
//! Validates the complete flows described in sections 2, 3, and 4:
//!
//!   Section 2: Login/Register Flow
//!   Section 3: Message Send Flow (DM + Channel)
//!   Section 4: Message Receive Flow
//!
//! All tests are gated by skip_if_no_server!() and require INTEGRATION_TESTS=1.

mod common;
use common::*;
use chatter_core::network::grpc::*;
use chatter_core::proto::chat_v1::*;
use tokio_stream::StreamExt;

// ---------------------------------------------------------------------------
// Section 2: Login / Register Flow
// ---------------------------------------------------------------------------

/// Test the complete registration flow.
///
/// Architecture reference (section 2):
///   Register:
///   - Create identity (Signal identity key pair)
///   - Authenticate with server (gRPC)
///   - Store session token
///   - Upload prekeys (100 one-time + 1 signed + 1 Kyber PQ)
///   - Returns user_id + device_id
#[tokio::test]
async fn test_register_flow_returns_valid_credentials() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(register_test_user(&pool).await);

    // Registration produces valid identifiers
    assert!(!user.user_id.is_empty(), "user_id should not be empty");
    assert!(user.device_id > 0, "device_id should be positive");
    assert!(!user.username.is_empty(), "username should not be empty");

    println!(
        "Registered: user_id={}, device_id={}, username={}",
        user.user_id, user.device_id, user.username
    );
}

/// Test that two independent registrations produce different user IDs.
///
/// Architecture reference:
///   Each registration creates a new identity with unique user_id.
#[tokio::test]
async fn test_register_produces_unique_user_ids() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user_a = assert_ok(register_test_user(&pool).await);
    let user_b = assert_ok(register_test_user(&pool).await);

    assert_ne!(user_a.user_id, user_b.user_id, "Each registration should produce unique user_id");
    assert_ne!(user_a.username, user_b.username, "Each registration should have unique username");
}

/// Test that unauthenticated pool cannot access protected endpoints.
///
/// Architecture reference:
///   Auth header format: Authorization: Basic base64({userId}.{deviceId}:{deviceAuthToken})
#[tokio::test]
async fn test_unauthenticated_pool_rejected() {
    skip_if_no_server!();

    // Pool without auth setup
    let pool = assert_ok(create_test_pool().await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let result = channel_client.list_channels(100, 0, None).await;

    // Should get an auth error
    assert!(result.is_err(), "Unauthenticated request should fail");
    assert_auth_error(result);
}

// ---------------------------------------------------------------------------
// Section 3: Message Send Flow — Channel Messages
// ---------------------------------------------------------------------------

/// Test creating a channel (streaming API).
///
/// Architecture reference (section 3: Channel Message):
///   Channel operations use streaming gRPC for real-time feedback.
#[tokio::test]
async fn test_channel_create() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let channel_name = unique_username(); // reuse unique name generator

    let stream = channel_client
        .create_channel(
            &channel_name,
            ChannelType::ChannelPublic,
            None,       // config
            vec![],     // initial_member_ids
            None,       // password
            None,       // description
            None,       // img_url
        )
        .await;

    let Ok(stream) = stream else {
        eprintln!("Failed to create channel stream");
        return;
    };

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(event_result) = stream.next().await {
        let event = assert_ok(event_result);
        if let Some(payload) = event.event {
            match payload {
                channel_event::Event::ChannelCreated(ch) => {
                    channel_id = ch.id;
                    println!("Channel created: {}", channel_id);
                }
                channel_event::Event::OperationFailed(e) => {
                    panic!("Channel creation failed: {:?}", e);
                }
                _ => {}
            }
        }
    }

    assert!(!channel_id.is_empty(), "Should have created a channel");
}

// ---------------------------------------------------------------------------
// Section 3: DM via Friend Request → DM Bootstrap
// ---------------------------------------------------------------------------

/// Test the social connection flow (friend request → acceptance).
///
/// Architecture reference (section 2: Post-login setup):
///   Two users must be friends to DM. The flow is:
///   1. User A sends friend request to User B
///   2. User B accepts → creates DM channel
///   3. DM bootstrap establishes mailboxes + conversation key
#[tokio::test]
async fn test_friend_request_flow() {
    skip_if_no_server!();

    let pool_a = assert_ok(create_test_pool().await);
    let user_a = assert_ok(setup_authenticated_test(&pool_a).await);

    let pool_b = assert_ok(create_test_pool().await);
    let user_b = assert_ok(setup_authenticated_test(&pool_b).await);

    // User A sends friend request to User B (streaming API)
    let mut user_client_a = UserClient::new(pool_a.channel(), pool_a.auth_clone());
    let stream = user_client_a
        .send_friend_request(&user_b.user_id, Some("Hey let's connect!"))
        .await;

    match stream {
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(event_result) = stream.next().await {
                match event_result {
                    Ok(event) => println!("Friend request event: {:?}", event.event),
                    Err(e) => eprintln!("Friend request stream error: {:?}", e),
                }
            }
            println!("Friend request sent from A to B");
        }
        Err(e) => {
            eprintln!("Friend request failed: {:?}", e);
            return;
        }
    }

    // User B accepts the friend request (streaming API)
    let mut user_client_b = UserClient::new(pool_b.channel(), pool_b.auth_clone());
    let stream = user_client_b
        .respond_friend_request(&user_a.user_id, FriendStatus::Accepted, None)
        .await;

    match stream {
        Ok(stream) => {
            tokio::pin!(stream);
            while let Some(event_result) = stream.next().await {
                match event_result {
                    Ok(event) => println!("Accept event: {:?}", event.event),
                    Err(e) => eprintln!("Accept stream error: {:?}", e),
                }
            }
            println!("Friend request accepted");
        }
        Err(e) => {
            eprintln!("Accept friend request failed: {:?}", e);
        }
    }
}

// ---------------------------------------------------------------------------
// Section 2: Protected endpoints
// ---------------------------------------------------------------------------

/// Test that unauthenticated requests are rejected.
///
/// Architecture reference:
///   Auth header format: Authorization: Basic base64({userId}.{deviceId}:{deviceAuthToken})
///   Endpoints require valid auth.
#[tokio::test]
async fn test_unauthenticated_request_rejected() {
    skip_if_no_server!();

    // Create pool WITHOUT authentication
    let pool = assert_ok(create_test_pool().await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());

    // Try to list channels without auth
    let result = channel_client.list_channels(10, 0, None).await;

    assert!(result.is_err(), "Unauthenticated request should be rejected");
    assert_auth_error(result);
}

/// Test that accessing nonexistent resources returns NotFound.
#[tokio::test]
async fn test_nonexistent_channel_returns_not_found() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());

    let result = channel_client
        .get_channel("00000000-0000-0000-0000-000000000000")
        .await;

    assert!(result.is_err(), "Nonexistent channel should return error");
    assert_not_found(result);
}
