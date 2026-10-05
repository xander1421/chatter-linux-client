//! Unit tests for network layer and simulation.
//!
//! Tests network conditions, mock server behavior,
//! and error handling using the testing framework.

use crate::framework;

use framework::prelude::*;
use std::time::Duration;

// ============================================================================
// Network Simulator Tests
// ============================================================================

#[tokio::test]
async fn test_perfect_network() {
    use framework::mocks::NetworkSimulator;

    let network = NetworkSimulator::perfect();

    // Perfect network should never fail
    for _ in 0..100 {
        assert!(!network.should_fail(), "Perfect network should not fail");
    }
}

#[tokio::test]
async fn test_offline_network() {
    use framework::mocks::NetworkSimulator;

    let network = NetworkSimulator::offline();

    // Offline network should always fail
    for _ in 0..10 {
        assert!(network.should_fail(), "Offline network should always fail");
    }
}

#[tokio::test]
async fn test_flaky_network() {
    use framework::mocks::NetworkSimulator;

    let network = NetworkSimulator::flaky(0.5); // 50% failure rate

    // Run many iterations to verify probabilistic behavior
    let mut failures = 0;
    let iterations = 1000;

    for _ in 0..iterations {
        if network.should_fail() {
            failures += 1;
        }
    }

    // With 50% rate, expect roughly 40-60% failures
    let failure_rate = failures as f32 / iterations as f32;
    assert!(
        failure_rate > 0.3 && failure_rate < 0.7,
        "Flaky network should fail roughly 50% of the time, got {:.2}%",
        failure_rate * 100.0
    );
}

#[tokio::test]
async fn test_network_with_latency() {
    use framework::mocks::NetworkSimulator;

    let network = NetworkSimulator::perfect().with_latency(Duration::from_millis(10));

    let start = std::time::Instant::now();
    network.simulate_latency().await;
    let elapsed = start.elapsed();

    assert!(
        elapsed >= Duration::from_millis(10),
        "Latency should be at least 10ms"
    );
}

#[tokio::test]
async fn test_network_bandwidth_simulation() {
    use framework::mocks::NetworkSimulator;

    // 1 MB/s bandwidth limit
    let network = NetworkSimulator::perfect().with_bandwidth_limit(1_000_000);

    let start = std::time::Instant::now();
    network.simulate_transfer(100_000).await; // 100KB
    let elapsed = start.elapsed();

    // 100KB at 1MB/s should take ~100ms
    assert!(
        elapsed >= Duration::from_millis(90),
        "Transfer should respect bandwidth limit"
    );
}

#[tokio::test]
async fn test_network_execute_success() {
    use framework::mocks::{MockResult, NetworkSimulator};

    let network = NetworkSimulator::perfect();

    let result: MockResult<i32> = network.execute(|| async { Ok(42) }).await;

    assert_eq!(result.unwrap(), 42);
}

#[tokio::test]
async fn test_network_execute_failure() {
    use framework::mocks::{MockError, NetworkSimulator};

    let network = NetworkSimulator::offline();

    let result: Result<i32, MockError> = network.execute(|| async { Ok(42) }).await;

    assert!(result.is_err(), "Offline network should cause failure");
    match result {
        Err(MockError::NetworkError(_)) => {}
        _ => panic!("Expected NetworkError"),
    }
}

// ============================================================================
// Mock gRPC Server Tests
// ============================================================================

#[tokio::test]
async fn test_mock_server_user_operations() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    // Register user
    let user = UserBuilder::named("TestUser").build();
    let user_id = server.register_user(user.clone());

    // Get user
    let retrieved = server.get_user(&user_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().display_name, "TestUser");
}

#[tokio::test]
async fn test_mock_server_channel_operations() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    let channel = ChannelBuilder::public("test-channel")
        .with_description("A test channel")
        .build();

    let channel_id = server.create_channel(channel);

    let retrieved = server.get_channel(&channel_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().name, "test-channel");
}

#[tokio::test]
async fn test_mock_server_message_operations() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    // Create channel first
    let channel = ChannelBuilder::public("chat").build();
    let channel_id = server.create_channel(channel);

    // Store messages
    let msg1 = MessageBuilder::text("Hello")
        .in_channel(&channel_id)
        .build();
    let msg2 = MessageBuilder::text("World")
        .in_channel(&channel_id)
        .build();

    server.store_message(msg1);
    server.store_message(msg2);

    // Get messages
    let messages = server.get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 2);
}

#[tokio::test]
async fn test_mock_server_session_operations() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    // Create session
    let token = server.create_session("user-123", 1);
    assert!(!token.is_empty());

    // Validate session
    let session = server.validate_session(&token);
    assert!(session.is_some());

    let (user_id, device_id) = session.unwrap();
    assert_eq!(user_id, "user-123");
    assert_eq!(device_id, 1);
}

#[tokio::test]
async fn test_mock_server_clear() {
    use framework::mocks::MockGrpcServer;

    let server = MockGrpcServer::new();

    // Add data
    let user = UserBuilder::named("User").build();
    let user_id = server.register_user(user);

    let channel = ChannelBuilder::public("channel").build();
    let channel_id = server.create_channel(channel);

    let token = server.create_session(&user_id, 1);

    // Clear everything
    server.clear();

    // All data should be gone
    assert!(server.get_user(&user_id).is_none());
    assert!(server.get_channel(&channel_id).is_none());
    assert!(server.validate_session(&token).is_none());
}

#[tokio::test]
async fn test_mock_server_with_network_conditions() {
    use framework::mocks::{MockGrpcServer, NetworkSimulator};

    let server =
        MockGrpcServer::new().with_network(NetworkSimulator::slow(Duration::from_millis(10)));

    // Server should still work, just with latency
    let user = UserBuilder::named("SlowUser").build();
    let user_id = server.register_user(user);

    assert!(server.get_user(&user_id).is_some());
}

// ============================================================================
// Harness Network Mode Tests
// ============================================================================

#[tokio::test]
async fn test_harness_mock_network() {
    let harness = TestHarness::builder().with_mock_network().build().await;

    assert!(
        harness.mock_server.is_some(),
        "Mock network should have mock server"
    );
}

#[tokio::test]
async fn test_harness_offline_mode() {
    let harness = TestHarness::builder().offline().build().await;

    // In offline mode, network operations should fail
    assert!(harness.network.should_fail());
}

#[tokio::test]
async fn test_harness_flaky_network() {
    let harness = TestHarness::builder().with_flaky_network(0.3).build().await;

    // Should have mock server even with flaky network
    assert!(harness.mock_server.is_some());
}

#[tokio::test]
async fn test_harness_with_latency() {
    let harness = TestHarness::builder()
        .with_mock_network()
        .with_latency(Duration::from_millis(5))
        .build()
        .await;

    let start = std::time::Instant::now();
    harness.network.simulate_latency().await;
    let elapsed = start.elapsed();

    assert!(elapsed >= Duration::from_millis(5));
}

// ============================================================================
// Error Type Tests
// ============================================================================

#[tokio::test]
async fn test_mock_error_not_found() {
    use framework::mocks::MockError;

    let error = MockError::NotFound("user".to_string());
    assert!(error.to_string().contains("Not found"));
    assert!(error.to_string().contains("user"));
}

#[tokio::test]
async fn test_mock_error_invalid_request() {
    use framework::mocks::MockError;

    let error = MockError::InvalidRequest("missing field".to_string());
    assert!(error.to_string().contains("Invalid request"));
}

#[tokio::test]
async fn test_mock_error_unauthorized() {
    use framework::mocks::MockError;

    let error = MockError::Unauthorized;
    assert!(error.to_string().contains("Unauthorized"));
}

#[tokio::test]
async fn test_mock_error_permission_denied() {
    use framework::mocks::MockError;

    let error = MockError::PermissionDenied;
    assert!(error.to_string().contains("Permission denied"));
}

#[tokio::test]
async fn test_mock_error_network() {
    use framework::mocks::MockError;

    let error = MockError::NetworkError("connection refused".to_string());
    assert!(error.to_string().contains("Network error"));
}

#[tokio::test]
async fn test_mock_error_crypto() {
    use framework::mocks::MockError;

    let error = MockError::CryptoError("invalid key".to_string());
    assert!(error.to_string().contains("Crypto error"));
}

#[tokio::test]
async fn test_mock_error_internal() {
    use framework::mocks::MockError;

    let error = MockError::Internal("unexpected error".to_string());
    assert!(error.to_string().contains("Internal error"));
}

// ============================================================================
// Network Simulation Integration Tests
// ============================================================================

#[tokio::test]
async fn test_complete_flow_with_network_sim() {
    let harness = TestHarness::builder()
        .with_mock_network()
        .with_latency(Duration::from_millis(1))
        .build()
        .await;

    let (alice, bob) = harness.conversation_pair().await;

    // Create channel
    let channel = ChannelBuilder::private("team")
        .created_by(alice.user_id())
        .with_member(bob.user_id())
        .build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Send messages through simulated network
    for i in 0..5 {
        let msg = MessageBuilder::text(&format!("Message {}", i))
            .in_channel(&channel_id)
            .from(if i % 2 == 0 {
                alice.user_id()
            } else {
                bob.user_id()
            })
            .build();
        harness.mock_server().store_message(msg);
    }

    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 5);
}

// ============================================================================
// Retry and Timeout Tests
// ============================================================================

#[tokio::test]
async fn test_network_failure_rate_boundaries() {
    use framework::mocks::NetworkSimulator;

    // 0% failure rate = never fails
    let network_0 = NetworkSimulator::flaky(0.0);
    for _ in 0..100 {
        assert!(!network_0.should_fail());
    }

    // 100% failure rate = always fails
    let network_100 = NetworkSimulator::flaky(1.0);
    for _ in 0..100 {
        assert!(network_100.should_fail());
    }
}

#[tokio::test]
async fn test_network_failure_rate_clamping() {
    use framework::mocks::NetworkSimulator;

    // Values outside 0-1 should be clamped
    let network_negative = NetworkSimulator::flaky(-0.5);
    for _ in 0..10 {
        assert!(
            !network_negative.should_fail(),
            "Negative rate should be clamped to 0"
        );
    }

    let network_over = NetworkSimulator::flaky(2.0);
    for _ in 0..10 {
        assert!(
            network_over.should_fail(),
            "Rate > 1 should be clamped to 1"
        );
    }
}
