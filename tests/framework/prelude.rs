//! Prelude module - import everything you need for testing.
//!
//! # Usage
//!
//! ```rust
//! use crate::framework::prelude::*;
//! ```

// Re-export test macros
pub use crate::framework::macros::*;

// Re-export builders
pub use crate::framework::builders::{
    UserBuilder, MessageBuilder, ChannelBuilder, AttachmentBuilder,
    CryptoBuilder, SessionBuilder,
};

// Re-export harness
pub use crate::framework::harness::{
    TestHarness, TestHarnessBuilder, UserContext,
    NetworkMode, CryptoMode, StorageMode,
};

// Re-export mocks
pub use crate::framework::mocks::MockServices;

// Re-export fixtures
pub use crate::framework::fixtures::{
    TestUser, TestChannel, TestMessage, TestAttachment, TestSession,
    MessageContentType,
    unique_username, random_password, random_password_hash,
    random_public_key, random_identity_key, random_device_auth_token,
    random_profile_key, random_uak, generate_prekey_bundle,
    unique_channel_name, unique_message_id, random_bytes, fake_cdn_url,
};

// Re-export profile crypto utilities for tests
pub use chatter_linux::crypto::profile_crypto::{
    encrypt_display_name, decrypt_display_name,
};
pub use chatter_linux::crypto::derive_uak;

// Re-export traits
pub use crate::framework::traits::{TestContext, Resettable, AsyncTestExt};

// Re-export from common (existing utilities)
pub use crate::common::{
    assert_ok, assert_not_found, assert_invalid_request,
    assert_unauthenticated, assert_permission_denied,
    assert_auth_error, assert_retryable, assert_server_error,
    create_test_pool, create_streaming_test_pool,
    register_test_user, setup_authenticated_test,
    server_url, should_skip_integration_tests,
    // Profile crypto test helpers
    encrypt_test_display_name, encrypt_test_display_name_base64,
    fake_encrypted_display_name, fake_encrypted_display_name_base64,
};

// Re-export common external crates used in tests
pub use tokio;
pub use tokio_stream::StreamExt;
pub use std::sync::Arc;
pub use std::time::Duration;

// Re-export chatter types commonly used in tests
pub use chatter_linux::error::NetworkError;
pub use chatter_linux::network::grpc::{
    AuthClient, ChannelClient, MessageClient, UserClient, GrpcClientPool,
};
pub use chatter_linux::proto::chat_v1::{
    ChannelType, FriendStatus, MessageSortDirection,
    channel_event, user_event, message,
};
