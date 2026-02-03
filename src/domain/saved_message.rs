//! Saved message domain entity.
//!
//! Represents messages saved to personal vault. Users can save text, files,
//! links, or entire messages from conversations for later reference.

use crate::domain::ids::{AttachmentId, ChannelId, MessageId, SavedMessageId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// SavedContent
// ============================================================================

/// Content variants for saved items.
///
/// Each variant stores the actual data needed to display the saved item
/// independently of the original source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SavedContent {
    /// Saved text content.
    Text {
        /// The text content that was saved.
        content: String,
    },
    /// Saved file reference.
    File {
        /// Reference to the attachment.
        attachment_id: AttachmentId,
        /// Original filename for display.
        filename: String,
    },
    /// Saved link/URL.
    Link {
        /// The URL that was saved.
        url: String,
        /// Optional title for the link.
        title: Option<String>,
    },
    /// Full saved message reference.
    Message {
        /// Reference to the original message.
        message_id: MessageId,
    },
}

impl SavedContent {
    /// Create a text saved content.
    #[must_use]
    pub fn text(content: String) -> Self {
        Self::Text { content }
    }

    /// Create a file saved content.
    #[must_use]
    pub fn file(attachment_id: AttachmentId, filename: String) -> Self {
        Self::File {
            attachment_id,
            filename,
        }
    }

    /// Create a link saved content.
    #[must_use]
    pub fn link(url: String, title: Option<String>) -> Self {
        Self::Link { url, title }
    }

    /// Create a message saved content.
    #[must_use]
    pub fn message(message_id: MessageId) -> Self {
        Self::Message { message_id }
    }
}

// ============================================================================
// SavedMessage
// ============================================================================

/// A message saved to the user's personal vault.
///
/// Saved messages allow users to bookmark important content from conversations
/// for later reference. They support:
/// - Multiple content types (text, files, links, full messages)
/// - Source tracking (which channel/message it came from)
/// - Personal notes
/// - Custom tags for organization
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedMessage {
    /// Unique identifier for this saved message.
    pub id: SavedMessageId,
    /// User who saved this message.
    pub user_id: UserId,
    /// The saved content.
    pub content: SavedContent,
    /// Channel where the content originated (if applicable).
    pub source_channel_id: Option<ChannelId>,
    /// Original message ID (if saved from a message).
    pub source_message_id: Option<MessageId>,
    /// User's personal note about this saved item.
    pub note: Option<String>,
    /// User-defined tags for organization.
    pub tags: Vec<String>,
    /// When the item was saved.
    pub saved_at: DateTime<Utc>,
}

impl SavedMessage {
    /// Create a new saved message.
    ///
    /// # Arguments
    /// * `user_id` - The user saving the content
    /// * `content` - The content to save
    #[must_use]
    pub fn new(user_id: UserId, content: SavedContent) -> Self {
        Self {
            id: SavedMessageId::new(),
            user_id,
            content,
            source_channel_id: None,
            source_message_id: None,
            note: None,
            tags: Vec::new(),
            saved_at: Utc::now(),
        }
    }

    /// Set the source channel and message for this saved item.
    ///
    /// Useful for tracking where content came from.
    #[must_use]
    pub fn with_source(mut self, channel_id: ChannelId, message_id: MessageId) -> Self {
        self.source_channel_id = Some(channel_id);
        self.source_message_id = Some(message_id);
        self
    }

    /// Add a tag to this saved message.
    ///
    /// Tags are normalized to lowercase and duplicates are ignored.
    pub fn add_tag(&mut self, tag: String) {
        let normalized = tag.to_lowercase();
        if !self.tags.iter().any(|t| t.to_lowercase() == normalized) {
            self.tags.push(tag);
        }
    }

    /// Remove a tag from this saved message.
    ///
    /// Comparison is case-insensitive.
    pub fn remove_tag(&mut self, tag: &str) {
        let normalized = tag.to_lowercase();
        self.tags.retain(|t| t.to_lowercase() != normalized);
    }

    /// Check if this saved message has a specific tag.
    ///
    /// Comparison is case-insensitive.
    #[must_use]
    pub fn has_tag(&self, tag: &str) -> bool {
        let normalized = tag.to_lowercase();
        self.tags.iter().any(|t| t.to_lowercase() == normalized)
    }

    /// Set or clear the personal note.
    pub fn set_note(&mut self, note: Option<String>) {
        self.note = note;
    }

    /// Get the content type as a string.
    ///
    /// Useful for filtering and display purposes.
    #[must_use]
    pub fn content_type(&self) -> &'static str {
        match &self.content {
            SavedContent::Text { .. } => "text",
            SavedContent::File { .. } => "file",
            SavedContent::Link { .. } => "link",
            SavedContent::Message { .. } => "message",
        }
    }

    /// Check if this saved message has a source.
    #[must_use]
    pub fn has_source(&self) -> bool {
        self.source_channel_id.is_some() || self.source_message_id.is_some()
    }

    /// Check if this saved message has a note.
    #[must_use]
    pub fn has_note(&self) -> bool {
        self.note.as_ref().is_some_and(|n| !n.trim().is_empty())
    }

    /// Check if this saved message has any tags.
    #[must_use]
    pub fn has_tags(&self) -> bool {
        !self.tags.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_saved_content_constructors() {
        let text = SavedContent::text("Hello, world!".to_string());
        assert!(matches!(text, SavedContent::Text { content } if content == "Hello, world!"));

        let file = SavedContent::file(AttachmentId::new(), "document.pdf".to_string());
        assert!(matches!(file, SavedContent::File { filename, .. } if filename == "document.pdf"));

        let link = SavedContent::link("https://example.com".to_string(), Some("Example".to_string()));
        assert!(matches!(link, SavedContent::Link { url, title } if url == "https://example.com" && title == Some("Example".to_string())));

        let message = SavedContent::message(MessageId::new());
        assert!(matches!(message, SavedContent::Message { .. }));
    }

    #[test]
    fn test_saved_message_new() {
        let user_id = UserId::new();
        let content = SavedContent::text("Important note".to_string());
        let saved = SavedMessage::new(user_id, content);

        assert_eq!(saved.user_id, user_id);
        assert!(matches!(saved.content, SavedContent::Text { .. }));
        assert!(saved.source_channel_id.is_none());
        assert!(saved.source_message_id.is_none());
        assert!(saved.note.is_none());
        assert!(saved.tags.is_empty());
    }

    #[test]
    fn test_saved_message_with_source() {
        let user_id = UserId::new();
        let channel_id = ChannelId::new();
        let message_id = MessageId::new();
        let content = SavedContent::text("From channel".to_string());

        let saved = SavedMessage::new(user_id, content).with_source(channel_id, message_id);

        assert_eq!(saved.source_channel_id, Some(channel_id));
        assert_eq!(saved.source_message_id, Some(message_id));
        assert!(saved.has_source());
    }

    #[test]
    fn test_saved_message_tags() {
        let user_id = UserId::new();
        let content = SavedContent::text("Tagged content".to_string());
        let mut saved = SavedMessage::new(user_id, content);

        assert!(!saved.has_tags());

        // Add tags
        saved.add_tag("Important".to_string());
        saved.add_tag("Work".to_string());
        assert!(saved.has_tags());
        assert_eq!(saved.tags.len(), 2);

        // Check tag existence (case-insensitive)
        assert!(saved.has_tag("important"));
        assert!(saved.has_tag("IMPORTANT"));
        assert!(saved.has_tag("work"));
        assert!(!saved.has_tag("personal"));

        // Adding duplicate (case-insensitive) should not add
        saved.add_tag("IMPORTANT".to_string());
        assert_eq!(saved.tags.len(), 2);

        // Remove tag (case-insensitive)
        saved.remove_tag("WORK");
        assert_eq!(saved.tags.len(), 1);
        assert!(!saved.has_tag("work"));
        assert!(saved.has_tag("important"));
    }

    #[test]
    fn test_saved_message_note() {
        let user_id = UserId::new();
        let content = SavedContent::text("Content".to_string());
        let mut saved = SavedMessage::new(user_id, content);

        assert!(!saved.has_note());

        saved.set_note(Some("My personal note".to_string()));
        assert!(saved.has_note());
        assert_eq!(saved.note, Some("My personal note".to_string()));

        // Empty note should not count as having a note
        saved.set_note(Some("   ".to_string()));
        assert!(!saved.has_note());

        saved.set_note(None);
        assert!(!saved.has_note());
    }

    #[test]
    fn test_saved_message_content_type() {
        let user_id = UserId::new();

        let text = SavedMessage::new(user_id, SavedContent::text("Text".to_string()));
        assert_eq!(text.content_type(), "text");

        let file = SavedMessage::new(
            user_id,
            SavedContent::file(AttachmentId::new(), "file.txt".to_string()),
        );
        assert_eq!(file.content_type(), "file");

        let link = SavedMessage::new(
            user_id,
            SavedContent::link("https://example.com".to_string(), None),
        );
        assert_eq!(link.content_type(), "link");

        let message = SavedMessage::new(user_id, SavedContent::message(MessageId::new()));
        assert_eq!(message.content_type(), "message");
    }
}
