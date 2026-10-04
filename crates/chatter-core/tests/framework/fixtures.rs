//! Extended test fixtures.
//!
//! Builds on common/fixtures.rs with additional test data types.

use rand::RngCore;

// Re-export everything from common fixtures
pub use crate::common::fixtures::*;

/// Test channel fixture.
#[derive(Debug, Clone)]
pub struct TestChannel {
    /// Channel ID.
    pub id: String,
    /// Channel name.
    pub name: String,
    /// Channel type.
    pub channel_type: i32,
    /// Description.
    pub description: Option<String>,
    /// Creator user ID.
    pub creator_id: String,
    /// Member user IDs.
    pub member_ids: Vec<String>,
    /// Creation timestamp.
    pub created_at: i64,
}

impl TestChannel {

}

/// Test message fixture.
#[derive(Debug, Clone)]
pub struct TestMessage {
    /// Message ID.
    pub id: String,
    /// Channel ID.
    pub channel_id: String,
    /// Sender user ID.
    pub sender_id: String,
    /// Message content.
    pub content: String,
    /// Reply to message ID.
    pub reply_to: Option<String>,
    /// Timestamp.
    pub timestamp: i64,
    /// Is encrypted.
    pub encrypted: bool,
    /// Attachments.
    pub attachments: Vec<TestAttachment>,
    /// Reactions (emoji -> user_ids).
    pub reactions: std::collections::HashMap<String, Vec<String>>,
}

/// Test attachment fixture.
#[derive(Debug, Clone)]
pub struct TestAttachment {
    /// Attachment ID.
    pub id: String,
    /// File name.
    pub filename: String,
    /// MIME type.
    pub mime_type: String,
    /// File size in bytes.
    pub size: u64,
    /// Encryption key (32 bytes).
    pub key: Vec<u8>,
    /// HMAC digest.
    pub digest: Vec<u8>,
    /// CDN URL.
    pub cdn_url: Option<String>,
}

impl TestAttachment {
    /// Create an image attachment.
    pub fn image(filename: impl Into<String>) -> Self {
        Self::new(filename, "image/jpeg", 1024 * 100) // 100KB
    }

    /// Create a video attachment.
    pub fn video(filename: impl Into<String>) -> Self {
        Self::new(filename, "video/mp4", 1024 * 1024 * 10) // 10MB
    }

    fn new(filename: impl Into<String>, mime_type: impl Into<String>, size: u64) -> Self {
        let mut key = vec![0u8; 32];
        let mut digest = vec![0u8; 32];
        rand::rng().fill_bytes(&mut key);
        rand::rng().fill_bytes(&mut digest);

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            filename: filename.into(),
            mime_type: mime_type.into(),
            size,
            key,
            digest,
            cdn_url: None,
        }
    }

}

/// Test Signal session fixture.
#[derive(Debug, Clone)]
pub struct TestSession {
    /// Remote user ID.
    pub user_id: String,
    /// Remote device ID.
    pub device_id: u32,
    /// Session state (serialized).
    pub session_data: Vec<u8>,
    /// Is session established.
    pub established: bool,
}

impl TestSession {

}

/// Generate a unique channel name.
pub fn unique_channel_name() -> String {
    let mut random_bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut random_bytes);
    format!("test-channel-{}", hex::encode(random_bytes))
}

/// Generate random bytes of specified length.
pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// Generate a fake CDN URL.
pub fn fake_cdn_url() -> String {
    format!(
        "https://cdn.example.com/attachments/{}",
        uuid::Uuid::new_v4()
    )
}
