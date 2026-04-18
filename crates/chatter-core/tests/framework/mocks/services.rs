//! Mock service traits.
//!
//! Defines mockable traits for core services using mockall.
//! These traits mirror the actual service APIs but are designed for testing.
//!
//! Note: mockall doesn't handle `&str` in async traits well, so we use owned types.

use mockall::automock;
use mockall::predicate::*;
use std::collections::HashMap;
use crate::framework::fixtures::{TestMessage, TestChannel, TestUser, TestAttachment};

/// Result type for mock services.
pub type MockResult<T> = Result<T, MockError>;

/// Error type for mock services.
#[derive(Debug, Clone, PartialEq)]
pub enum MockError {
    NotFound(String),
    InvalidRequest(String),
    Unauthorized,
    PermissionDenied,
    NetworkError(String),
    CryptoError(String),
    Internal(String),
}

impl std::fmt::Display for MockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "Not found: {}", msg),
            Self::InvalidRequest(msg) => write!(f, "Invalid request: {}", msg),
            Self::Unauthorized => write!(f, "Unauthorized"),
            Self::PermissionDenied => write!(f, "Permission denied"),
            Self::NetworkError(msg) => write!(f, "Network error: {}", msg),
            Self::CryptoError(msg) => write!(f, "Crypto error: {}", msg),
            Self::Internal(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for MockError {}

// ============================================================================
// Message Service Trait
// ============================================================================

/// Mockable message service trait.
#[automock]
pub trait MessageServiceTrait: Send + Sync {
    /// Send a text message.
    fn send_message(
        &self,
        channel_id: String,
        content: String,
        reply_to: Option<String>,
    ) -> impl std::future::Future<Output = MockResult<TestMessage>> + Send;

    /// Send a message with attachments.
    fn send_with_attachments(
        &self,
        channel_id: String,
        content: String,
        attachments: Vec<TestAttachment>,
    ) -> impl std::future::Future<Output = MockResult<TestMessage>> + Send;

    /// Get messages from a channel.
    fn get_messages(
        &self,
        channel_id: String,
        limit: u32,
        before: Option<String>,
    ) -> impl std::future::Future<Output = MockResult<Vec<TestMessage>>> + Send;

    /// Get a single message.
    fn get_message(
        &self,
        message_id: String,
    ) -> impl std::future::Future<Output = MockResult<TestMessage>> + Send;

    /// Edit a message.
    fn edit_message(
        &self,
        message_id: String,
        new_content: String,
    ) -> impl std::future::Future<Output = MockResult<TestMessage>> + Send;

    /// Delete a message.
    fn delete_message(
        &self,
        message_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Add reaction to a message.
    fn add_reaction(
        &self,
        message_id: String,
        emoji: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Remove reaction from a message.
    fn remove_reaction(
        &self,
        message_id: String,
        emoji: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Pin a message.
    fn pin_message(
        &self,
        message_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Unpin a message.
    fn unpin_message(
        &self,
        message_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;
}

// ============================================================================
// Channel Service Trait
// ============================================================================

/// Mockable channel service trait.
#[automock]
pub trait ChannelServiceTrait: Send + Sync {
    /// Create a channel.
    fn create_channel(
        &self,
        name: String,
        channel_type: i32,
        description: Option<String>,
    ) -> impl std::future::Future<Output = MockResult<TestChannel>> + Send;

    /// Get a channel by ID.
    fn get_channel(
        &self,
        channel_id: String,
    ) -> impl std::future::Future<Output = MockResult<TestChannel>> + Send;

    /// List user's channels.
    fn list_channels(
        &self,
    ) -> impl std::future::Future<Output = MockResult<Vec<TestChannel>>> + Send;

    /// Update channel settings.
    fn update_channel(
        &self,
        channel_id: String,
        name: Option<String>,
        description: Option<String>,
    ) -> impl std::future::Future<Output = MockResult<TestChannel>> + Send;

    /// Delete a channel.
    fn delete_channel(
        &self,
        channel_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Join a channel.
    fn join_channel(
        &self,
        channel_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Leave a channel.
    fn leave_channel(
        &self,
        channel_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Invite user to channel.
    fn invite_user(
        &self,
        channel_id: String,
        user_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Get channel members.
    fn get_members(
        &self,
        channel_id: String,
    ) -> impl std::future::Future<Output = MockResult<Vec<String>>> + Send;
}

// ============================================================================
// User Service Trait
// ============================================================================

/// Mockable user service trait.
#[automock]
pub trait UserServiceTrait: Send + Sync {
    /// Get user profile.
    fn get_profile(
        &self,
        user_id: String,
    ) -> impl std::future::Future<Output = MockResult<TestUser>> + Send;

    /// Update user profile.
    fn update_profile(
        &self,
        display_name: Option<String>,
        avatar_url: Option<String>,
    ) -> impl std::future::Future<Output = MockResult<TestUser>> + Send;

    /// Send friend request.
    fn send_friend_request(
        &self,
        user_id: String,
        message: Option<String>,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Accept friend request.
    fn accept_friend_request(
        &self,
        user_id: String,
    ) -> impl std::future::Future<Output = MockResult<String>> + Send; // Returns DM channel ID

    /// Decline friend request.
    fn decline_friend_request(
        &self,
        user_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// List friends.
    fn list_friends(
        &self,
    ) -> impl std::future::Future<Output = MockResult<Vec<TestUser>>> + Send;

    /// Block user.
    fn block_user(
        &self,
        user_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Unblock user.
    fn unblock_user(
        &self,
        user_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Signal-style exact-username lookup. Returns the matched user id or
    /// `None` when no account is registered under the given username hash.
    fn lookup_username(
        &self,
        username_hash: [u8; 32],
    ) -> impl std::future::Future<Output = MockResult<Option<String>>> + Send;
}

// ============================================================================
// Auth Service Trait
// ============================================================================

/// Mockable auth service trait.
#[automock]
pub trait AuthServiceTrait: Send + Sync {
    /// Login with username and password.
    fn login(
        &self,
        username: String,
        password_hash: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<(String, u32)>> + Send; // (user_id, device_id)

    /// Register new user.
    fn register(
        &self,
        username: String,
        password_hash: Vec<u8>,
        display_name: String,
    ) -> impl std::future::Future<Output = MockResult<(String, u32)>> + Send;

    /// Logout current session.
    fn logout(
        &self,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// Change password.
    fn change_password(
        &self,
        old_hash: Vec<u8>,
        new_hash: Vec<u8>,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    /// List active sessions.
    fn list_sessions(
        &self,
    ) -> impl std::future::Future<Output = MockResult<Vec<(u32, String)>>> + Send; // (device_id, device_info)

    /// Revoke a session.
    fn revoke_session(
        &self,
        device_id: u32,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;
}

// ============================================================================
// Attachment Service Trait
// ============================================================================

/// Mockable attachment service trait.
#[automock]
pub trait AttachmentServiceTrait: Send + Sync {
    /// Upload an attachment.
    fn upload(
        &self,
        data: Vec<u8>,
        filename: String,
        mime_type: String,
    ) -> impl std::future::Future<Output = MockResult<TestAttachment>> + Send;

    /// Download an attachment.
    fn download(
        &self,
        attachment_id: String,
    ) -> impl std::future::Future<Output = MockResult<Vec<u8>>> + Send;

    /// Get attachment metadata.
    fn get_metadata(
        &self,
        attachment_id: String,
    ) -> impl std::future::Future<Output = MockResult<TestAttachment>> + Send;

    /// Delete an attachment.
    fn delete(
        &self,
        attachment_id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;
}
