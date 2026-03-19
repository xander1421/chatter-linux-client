//! Message builder for creating test messages.

use crate::framework::fixtures::{TestMessage, TestAttachment, MessageContentType};

/// Builder for creating test messages.
pub struct MessageBuilder {
    content: String,
    content_type: MessageContentType,
    channel_id: Option<String>,
    sender_id: Option<String>,
    reply_to: Option<String>,
    thread_root: Option<String>,
    encrypted: bool,
    attachments: Vec<TestAttachment>,
    reactions: std::collections::HashMap<String, Vec<String>>,
    timestamp: Option<i64>,
}

impl MessageBuilder {
    /// Create a text message builder.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            content_type: MessageContentType::Text,
            channel_id: None,
            sender_id: None,
            reply_to: None,
            thread_root: None,
            encrypted: true,
            attachments: Vec::new(),
            reactions: std::collections::HashMap::new(),
            timestamp: None,
        }
    }

    /// Create an image message builder.
    pub fn image() -> Self {
        Self {
            content: String::new(),
            content_type: MessageContentType::Image,
            channel_id: None,
            sender_id: None,
            reply_to: None,
            thread_root: None,
            encrypted: true,
            attachments: vec![TestAttachment::image("image.jpg")],
            reactions: std::collections::HashMap::new(),
            timestamp: None,
        }
    }

    /// Create a video message builder.
    pub fn video() -> Self {
        Self {
            content: String::new(),
            content_type: MessageContentType::Video,
            channel_id: None,
            sender_id: None,
            reply_to: None,
            thread_root: None,
            encrypted: true,
            attachments: vec![TestAttachment::video("video.mp4")],
            reactions: std::collections::HashMap::new(),
            timestamp: None,
        }
    }

    /// Set the channel.
    pub fn in_channel(mut self, channel_id: impl Into<String>) -> Self {
        self.channel_id = Some(channel_id.into());
        self
    }

    /// Set the sender.
    pub fn from(mut self, user_id: impl Into<String>) -> Self {
        self.sender_id = Some(user_id.into());
        self
    }

    /// Set as reply to another message.
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

    /// Add an attachment.
    pub fn with_attachment(mut self, attachment: TestAttachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// Add a reaction.
    pub fn with_reaction(mut self, emoji: impl Into<String>, user_id: impl Into<String>) -> Self {
        self.reactions
            .entry(emoji.into())
            .or_default()
            .push(user_id.into());
        self
    }

    /// Set specific timestamp.
    pub fn at_time(mut self, timestamp: i64) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    /// Build the message.
    pub fn build(self) -> TestMessage {
        TestMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel_id: self.channel_id.unwrap_or_default(),
            sender_id: self.sender_id.unwrap_or_default(),
            content: self.content,
            content_type: self.content_type,
            reply_to: self.reply_to,
            thread_root: self.thread_root,
            timestamp: self.timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
            encrypted: self.encrypted,
            attachments: self.attachments,
            reactions: self.reactions,
        }
    }

    /// Build multiple messages in sequence.
    pub fn build_conversation(messages: Vec<(&str, &str)>, channel_id: &str) -> Vec<TestMessage> {
        let base_time = chrono::Utc::now().timestamp_millis();
        messages.into_iter().enumerate().map(|(i, (sender, content))| {
            Self::text(content)
                .from(sender)
                .in_channel(channel_id)
                .at_time(base_time + (i as i64 * 1000))
                .build()
        }).collect()
    }
}
