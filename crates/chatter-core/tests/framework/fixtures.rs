//! Extended test fixtures.
//!
//! Builds on common/fixtures.rs with additional test data types.

use std::sync::Arc;
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
    /// Create a new test channel fixture.
    pub fn new(name: impl Into<String>, creator_id: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            channel_type: 1, // Public
            description: None,
            creator_id: creator_id.into(),
            member_ids: Vec::new(),
            created_at: chrono::Utc::now().timestamp_millis(),
        }
    }

    /// Set as private channel.
    pub fn private(mut self) -> Self {
        self.channel_type = 2;
        self
    }

    /// Set as DM channel.
    pub fn dm(mut self) -> Self {
        self.channel_type = 3;
        self
    }

    /// Add a member.
    pub fn with_member(mut self, user_id: impl Into<String>) -> Self {
        self.member_ids.push(user_id.into());
        self
    }

    /// Set description.
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }
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
    /// Content type.
    pub content_type: MessageContentType,
    /// Reply to message ID.
    pub reply_to: Option<String>,
    /// Thread root message ID.
    pub thread_root: Option<String>,
    /// Timestamp.
    pub timestamp: i64,
    /// Is encrypted.
    pub encrypted: bool,
    /// Attachments.
    pub attachments: Vec<TestAttachment>,
    /// Reactions (emoji -> user_ids).
    pub reactions: std::collections::HashMap<String, Vec<String>>,
}

/// Message content type.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageContentType {
    Text,
    Image,
    Video,
    Audio,
    File,
    Sticker,
    Gif,
}

impl TestMessage {
    /// Create a new text message.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            channel_id: String::new(),
            sender_id: String::new(),
            content: content.into(),
            content_type: MessageContentType::Text,
            reply_to: None,
            thread_root: None,
            timestamp: chrono::Utc::now().timestamp_millis(),
            encrypted: true,
            attachments: Vec::new(),
            reactions: std::collections::HashMap::new(),
        }
    }

    /// Set channel ID.
    pub fn in_channel(mut self, channel_id: impl Into<String>) -> Self {
        self.channel_id = channel_id.into();
        self
    }

    /// Set sender.
    pub fn from_user(mut self, user_id: impl Into<String>) -> Self {
        self.sender_id = user_id.into();
        self
    }

    /// Set as reply.
    pub fn reply_to(mut self, message_id: impl Into<String>) -> Self {
        self.reply_to = Some(message_id.into());
        self
    }

    /// Set as thread reply.
    pub fn in_thread(mut self, root_id: impl Into<String>) -> Self {
        self.thread_root = Some(root_id.into());
        self
    }

    /// Set as unencrypted.
    pub fn unencrypted(mut self) -> Self {
        self.encrypted = false;
        self
    }

    /// Add attachment.
    pub fn with_attachment(mut self, attachment: TestAttachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// Add reaction.
    pub fn with_reaction(mut self, emoji: impl Into<String>, user_id: impl Into<String>) -> Self {
        self.reactions
            .entry(emoji.into())
            .or_default()
            .push(user_id.into());
        self
    }
}

/// Test attachment fixture.
#[derive(Debug, Clone)]
#[allow(dead_code)]
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

    /// Create a file attachment.
    pub fn file(filename: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::new(filename, mime_type, 1024 * 50) // 50KB
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

    /// Set CDN URL.
    pub fn with_cdn_url(mut self, url: impl Into<String>) -> Self {
        self.cdn_url = Some(url.into());
        self
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
    /// Create a new unestablished session.
    pub fn new(user_id: impl Into<String>, device_id: u32) -> Self {
        Self {
            user_id: user_id.into(),
            device_id,
            session_data: Vec::new(),
            established: false,
        }
    }

    /// Mark as established with fake session data.
    pub fn established(mut self) -> Self {
        let mut data = vec![0u8; 256];
        rand::rng().fill_bytes(&mut data);
        self.session_data = data;
        self.established = true;
        self
    }
}

/// Generate a unique channel name.
pub fn unique_channel_name() -> String {
    let mut random_bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut random_bytes);
    format!("test-channel-{}", hex::encode(random_bytes))
}

/// Generate a unique message ID.
pub fn unique_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
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
